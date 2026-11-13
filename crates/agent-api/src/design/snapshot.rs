//! Stored designs (spec B2 §1, §2.4, D62): a job's design at every snapshot place, the stored baseline of a job never
//! released, and the release snapshots (a port of `fixtures/ledger.ts` `takeSnapshot` / `baselineOf` /
//! `snapshotOf` / `liveSnapshot`).
//!
//! Coverage (§2.5 "late registration"): a stored snapshot covers the kinds in its `snapshot_kinds`
//! (`jobs.baseline_kinds` for a baseline). A kind registered after the snapshot was stored is not covered, and every
//! read of it falls through to the working rows — exactly what `design::coverage::backfill` copies in at boot.
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

use super::{DesignOf, DraftRef, JobRef, Place, Registry, Snap, snap_from_json, snap_to_json};
use crate::error::{ApiError, ApiResult};

/// One design place and the value a design holds there (fixture `DesignStateRow`).
#[derive(Clone, Debug, PartialEq)]
pub struct DesignRow {
    pub key: String,
    pub kind: String,
    pub r#ref: DraftRef,
    pub snap: Snap,
}

/// A stored design: a release snapshot or a job's baseline. Immutable once loaded (the LRU of
/// [`super::read::SnapshotCache`] shares it).
#[derive(Debug)]
pub struct Snapshot {
    /// The release it belongs to; `None` for a baseline (or a job whose baseline is not taken yet).
    pub release_id: Option<String>,
    /// The kinds it covers (`snapshot_kinds` / `baseline_kinds`).
    kinds: Vec<String>,
    /// The release hash (cache validation); `None` for a baseline.
    hash: Option<String>,
    rows: Vec<DesignRow>,
    index: HashMap<String, usize>,
}

impl Snapshot {
    fn new(
        release_id: Option<String>,
        kinds: Vec<String>,
        hash: Option<String>,
        rows: Vec<DesignRow>,
    ) -> Self {
        let mut index = HashMap::with_capacity(rows.len());
        for (i, r) in rows.iter().enumerate() {
            index.entry(r.key.clone()).or_insert(i);
        }
        Self {
            release_id,
            kinds,
            hash,
            rows,
            index,
        }
    }

    /// Every stored place, ordered by key.
    pub fn rows(&self) -> &[DesignRow] {
        &self.rows
    }

    /// The stored place with `key`.
    pub fn row(&self, key: &str) -> Option<&DesignRow> {
        self.index.get(key).map(|&i| &self.rows[i])
    }

    /// Does this snapshot store `kind`'s places? Not covered = read the working rows (see the module doc).
    pub fn covers(&self, kind: &str) -> bool {
        self.kinds.iter().any(|k| k == kind)
    }

    /// The same stored release version: the cache key alone is not trusted (a release id can be reused after a
    /// rolled-back cut, and `snapshot_kinds` grows on backfill).
    pub(crate) fn same_version(&self, hash: &str, kinds: &[String]) -> bool {
        self.hash.as_deref() == Some(hash) && self.kinds == kinds
    }
}

/// The job columns every design read needs.
#[derive(Debug, sqlx::FromRow)]
pub(crate) struct JobState {
    pub tenant_id: Uuid,
    pub current_release_id: Option<String>,
    pub baseline_kinds: Option<Vec<String>>,
    pub draft_rev: i64,
}

pub(crate) fn job_not_found() -> ApiError {
    ApiError::Missing("job_not_found", "That job does not exist.".into())
}

pub(crate) fn internal(what: String) -> ApiError {
    ApiError::Internal(anyhow::anyhow!(what))
}

pub(crate) async fn job_state(c: &mut PgConnection, job: &JobRef) -> ApiResult<JobState> {
    sqlx::query_as::<_, JobState>(
        "SELECT tenant_id, current_release_id, baseline_kinds, draft_rev FROM jobs WHERE id = $1",
    )
    .bind(job.id)
    .fetch_optional(&mut *c)
    .await?
    .ok_or_else(job_not_found)
}

/// A stored ref (jsonb object) as a [`DraftRef`]; anything else is a corrupt row (500).
pub(crate) fn ref_of(v: Value) -> ApiResult<DraftRef> {
    match v {
        Value::Object(m) => Ok(m),
        other => Err(internal(format!(
            "stored design ref is not an object: {other}"
        ))),
    }
}

/// The working rows (the draft) at every place of `kinds` (fixture `takeSnapshot(world, …)`). A key seen twice
/// keeps its first place; a projection kind (no places) contributes nothing.
pub(crate) async fn working_rows(
    c: &mut PgConnection,
    reg: &Registry,
    tenant: Uuid,
    job: &JobRef,
    kinds: &[&'static str],
) -> ApiResult<Vec<DesignRow>> {
    let at = Place {
        tenant,
        job: Some(job),
    };
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    for &kind in kinds {
        let t = reg.target(kind)?;
        let Some(refs) = t.places(&mut *c, at).await? else {
            continue;
        };
        for r in refs {
            let key = t.key(&r);
            if !seen.insert(key.clone()) {
                continue;
            }
            let snap = t.get(&mut *c, at, &r).await?;
            out.push(DesignRow {
                key,
                kind: kind.to_string(),
                r#ref: r,
                snap,
            });
        }
    }
    Ok(out)
}

/// Take the stored baseline of a job never released (fixture `baselineOf`, §2.3 step 2): one `release_baseline` row
/// per place of every registered snapshot kind — tenant kinds included, read at `Place { job: Some(job) }` — and
/// `jobs.baseline_kinds` = those kinds. Idempotent: a job that has a baseline or a release is left alone. Call it
/// with the job row locked (`design::track` does, `SELECT … FOR UPDATE`) and BEFORE the working rows change.
pub async fn ensure_baseline(c: &mut PgConnection, reg: &Registry, job: &JobRef) -> ApiResult<()> {
    let st = job_state(&mut *c, job).await?;
    if st.current_release_id.is_some() || st.baseline_kinds.is_some() {
        return Ok(());
    }
    let kinds = reg.snapshot_kinds();
    let rows = working_rows(&mut *c, reg, st.tenant_id, job, &kinds).await?;
    for r in &rows {
        sqlx::query(
            "INSERT INTO release_baseline (tenant_id, job_id, key, kind, ref, snap) \
             VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (tenant_id, job_id, key) DO NOTHING",
        )
        .bind(st.tenant_id)
        .bind(job.id)
        .bind(&r.key)
        .bind(&r.kind)
        .bind(Value::Object(r.r#ref.clone()))
        .bind(snap_to_json(&r.snap))
        .execute(&mut *c)
        .await?;
    }
    let names: Vec<String> = kinds.iter().map(|k| k.to_string()).collect();
    sqlx::query("UPDATE jobs SET baseline_kinds = $2 WHERE id = $1")
        .bind(job.id)
        .bind(names)
        .execute(&mut *c)
        .await?;
    Ok(())
}

#[derive(sqlx::FromRow)]
struct StoredRow {
    key: String,
    kind: String,
    place_ref: Value,
    snap: Option<Value>,
}

fn rows_of(stored: Vec<StoredRow>) -> ApiResult<Vec<DesignRow>> {
    stored
        .into_iter()
        .map(|s| -> ApiResult<DesignRow> {
            Ok(DesignRow {
                key: s.key,
                kind: s.kind,
                r#ref: ref_of(s.place_ref)?,
                snap: snap_from_json(s.snap)?,
            })
        })
        .collect()
}

#[derive(sqlx::FromRow)]
struct ReleaseHead {
    hash: String,
    snapshot_kinds: Vec<String>,
}

/// A release's stored snapshot, through the LRU (`Registry::snapshots`). 404 `release_not_found` for a release the
/// job does not have.
async fn release_snapshot(
    c: &mut PgConnection,
    reg: &Registry,
    tenant: Uuid,
    job: &JobRef,
    release: &str,
) -> ApiResult<Arc<Snapshot>> {
    let head = sqlx::query_as::<_, ReleaseHead>(
        "SELECT hash, snapshot_kinds FROM job_release WHERE job_id = $1 AND release_id = $2",
    )
    .bind(job.id)
    .bind(release)
    .fetch_optional(&mut *c)
    .await?
    .ok_or_else(|| {
        ApiError::Missing(
            "release_not_found",
            format!("The job has no release {release}."),
        )
    })?;
    let key = (tenant, job.id, release.to_string());
    if let Some(hit) = reg.snapshots().get(&key, &head.hash, &head.snapshot_kinds) {
        return Ok(hit);
    }
    let stored = sqlx::query_as::<_, StoredRow>(
        "SELECT key, kind, ref AS place_ref, snap FROM release_snapshot \
          WHERE job_id = $1 AND release_id = $2 ORDER BY key",
    )
    .bind(job.id)
    .bind(release)
    .fetch_all(&mut *c)
    .await?;
    let snap = Arc::new(Snapshot::new(
        Some(release.to_string()),
        head.snapshot_kinds,
        Some(head.hash),
        rows_of(stored)?,
    ));
    reg.snapshots().put(key, snap.clone());
    Ok(snap)
}

/// The stored baseline (never cached, read per call). A job whose baseline is not taken yet (`kinds` `None`) has an
/// empty one that covers nothing: until the first tracked write, live = the working rows.
async fn baseline(
    c: &mut PgConnection,
    job: &JobRef,
    kinds: Option<Vec<String>>,
) -> ApiResult<Arc<Snapshot>> {
    let Some(kinds) = kinds else {
        return Ok(Arc::new(Snapshot::new(None, Vec::new(), None, Vec::new())));
    };
    let stored = sqlx::query_as::<_, StoredRow>(
        "SELECT key, kind, ref AS place_ref, snap FROM release_baseline WHERE job_id = $1 ORDER BY key",
    )
    .bind(job.id)
    .fetch_all(&mut *c)
    .await?;
    Ok(Arc::new(Snapshot::new(None, kinds, None, rows_of(stored)?)))
}

/// The stored design `of` names: `Live` = the current release's snapshot, else the baseline (fixture
/// `liveSnapshot`); `Release(id)` = that release's snapshot; `Draft` = `None` (the working rows are not stored).
pub(crate) async fn stored(
    c: &mut PgConnection,
    reg: &Registry,
    job: &JobRef,
    st: &JobState,
    of: &DesignOf,
) -> ApiResult<Option<Arc<Snapshot>>> {
    match of {
        DesignOf::Draft => Ok(None),
        DesignOf::Release(id) => release_snapshot(c, reg, st.tenant_id, job, id)
            .await
            .map(Some),
        DesignOf::Live => match &st.current_release_id {
            Some(id) => release_snapshot(c, reg, st.tenant_id, job, id)
                .await
                .map(Some),
            None => baseline(c, job, st.baseline_kinds.clone()).await.map(Some),
        },
    }
}

/// The job's design at every snapshot place (fixture `designStateOf` values): `Draft` = the working rows; `Live` /
/// `Release(id)` = the stored snapshot, with the working rows of every registered kind it does not cover.
pub async fn take_snapshot(
    c: &mut PgConnection,
    reg: &Registry,
    job: &JobRef,
    of: &DesignOf,
) -> ApiResult<Vec<DesignRow>> {
    let st = job_state(&mut *c, job).await?;
    let kinds = reg.snapshot_kinds();
    let Some(s) = stored(&mut *c, reg, job, &st, of).await? else {
        return working_rows(c, reg, st.tenant_id, job, &kinds).await;
    };
    let mut out: Vec<DesignRow> = s.rows().to_vec();
    let missing: Vec<&'static str> = kinds.into_iter().filter(|k| !s.covers(k)).collect();
    if !missing.is_empty() {
        let seen: BTreeSet<String> = out.iter().map(|r| r.key.clone()).collect();
        for r in working_rows(c, reg, st.tenant_id, job, &missing).await? {
            if !seen.contains(&r.key) {
                out.push(r);
            }
        }
    }
    Ok(out)
}
