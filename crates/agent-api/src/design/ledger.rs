//! The draft ledger (spec B2 §1, §2.3 step 5; D54/D62): the change HISTORY of a job's unpublished draft, one row per
//! changed place — who changed it, when, the value the first save replaced (`before`) and the draft value now
//! (`after`). It is never read back to rebuild live. A port of `fixtures/ledger.ts` `recordLedger` / `bundledIn` /
//! `ledgerOf` / `findDraft`.
use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

use super::read::{live_uncovered, same, value};
use super::snapshot::ref_of;
use super::{DesignOf, DraftRef, JobRef, Place, Registry, Snap, snap_from_json, snap_to_json};
use crate::error::ApiResult;

/// `draft_ledger.label` holds at most this many characters (CHECK `draft_ledger_label_check`); a longer label is
/// cut, never refused.
pub const MAX_LABEL_CHARS: usize = 500;

/// One ledger row (fixture `DraftRow`; API id `d-<id>`).
#[derive(Clone, Debug, PartialEq)]
pub struct LedgerRow {
    pub id: i64,
    pub job_id: Uuid,
    pub key: String,
    pub kind: String,
    pub r#ref: DraftRef,
    pub area: String,
    pub page: String,
    pub label: String,
    /// The value the first save replaced.
    pub before: Snap,
    /// The draft value now.
    pub after: Snap,
    pub at: DateTime<Utc>,
    pub by_sub: String,
    /// A rollback moved the live value under this change: `{release, at, was}`.
    pub live_moved: Option<Value>,
}

#[derive(sqlx::FromRow)]
struct Stored {
    id: i64,
    job_id: Uuid,
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
    live_moved: Option<Value>,
}

const COLUMNS: &str = "id, job_id, key, kind, ref AS place_ref, area, page, label, before, after, at, by_sub, live_moved";

impl Stored {
    fn row(self) -> ApiResult<LedgerRow> {
        Ok(LedgerRow {
            id: self.id,
            job_id: self.job_id,
            key: self.key,
            kind: self.kind,
            r#ref: ref_of(self.place_ref)?,
            area: self.area,
            page: self.page,
            label: self.label,
            before: snap_from_json(self.before)?,
            after: snap_from_json(self.after)?,
            at: self.at,
            by_sub: self.by_sub,
            live_moved: self.live_moved,
        })
    }
}

/// The job's ledger row at `key` (fixture `findDraft`; spec §2.7 ledger lookup).
pub async fn find(c: &mut PgConnection, job: &JobRef, key: &str) -> ApiResult<Option<LedgerRow>> {
    let row = sqlx::query_as::<_, Stored>(sqlx::AssertSqlSafe(format!(
        "SELECT {COLUMNS} FROM draft_ledger WHERE job_id = $1 AND key = $2"
    )))
    .bind(job.id)
    .bind(key)
    .fetch_optional(&mut *c)
    .await?;
    row.map(Stored::row).transpose()
}

/// The job's ledger rows, least recently written first (fixture `ledgerOf`).
pub async fn of_job(c: &mut PgConnection, job: &JobRef) -> ApiResult<Vec<LedgerRow>> {
    let rows = sqlx::query_as::<_, Stored>(sqlx::AssertSqlSafe(format!(
        "SELECT {COLUMNS} FROM draft_ledger WHERE job_id = $1 ORDER BY at, id"
    )))
    .bind(job.id)
    .fetch_all(&mut *c)
    .await?;
    rows.into_iter().map(Stored::row).collect()
}

/// Does an approved (cut, not yet applied) release of the job bundle `key`? (fixture `bundledIn`, controller ruling
/// R6: a `release_bundled_draft` row for the key whose `job_release.state = 'approved'`.)
pub async fn bundled_in_approved(c: &mut PgConnection, job: &JobRef, key: &str) -> ApiResult<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM release_bundled_draft b \
                          JOIN job_release r ON r.tenant_id = b.tenant_id AND r.job_id = b.job_id \
                                            AND r.release_id = b.release_id \
                         WHERE b.job_id = $1 AND b.key = $2 AND r.state = 'approved')",
    )
    .bind(job.id)
    .bind(key)
    .fetch_one(&mut *c)
    .await?)
}

/// The approved (cut, not yet applied) releases of the job that bundle `key`, oldest cut first (fixture `bundledIn`;
/// a change row's `in_release` is the first).
pub async fn approved_cuts(
    c: &mut PgConnection,
    job: &JobRef,
    key: &str,
) -> ApiResult<Vec<String>> {
    Ok(sqlx::query_scalar(
        "SELECT r.release_id FROM release_bundled_draft b \
           JOIN job_release r ON r.tenant_id = b.tenant_id AND r.job_id = b.job_id \
                             AND r.release_id = b.release_id \
          WHERE b.job_id = $1 AND b.key = $2 AND r.state = 'approved' \
          ORDER BY r.approved_at, r.release_id",
    )
    .bind(job.id)
    .bind(key)
    .fetch_all(&mut *c)
    .await?)
}

/// One change key (fixture J17 `changeKey(key, after)`): `<key>@<after as JSON>`, `after` in its stored form
/// (`{"v": …}`, or `null` for nothing at the place). Object keys serialise sorted (serde_json), where the fixture keeps
/// insertion order: a fingerprint is compared only with fingerprints this API computed.
pub fn change_key(key: &str, after: &Snap) -> String {
    let stored = snap_to_json(after).unwrap_or(Value::Null);
    format!("{key}@{stored}")
}

/// The job's change keys, one per ledger row, oldest first (spec §2.7 draft fingerprint: B4/B8 hash them with
/// [`super::fingerprint`]). A changed draft value changes its key.
pub async fn keys(c: &mut PgConnection, job: &JobRef) -> ApiResult<Vec<String>> {
    Ok(of_job(c, job)
        .await?
        .iter()
        .map(|r| change_key(&r.key, &r.after))
        .collect())
}

/// What one write records (the [`super::track::DraftWrite`] fields, the label already rendered).
pub(crate) struct Entry<'a> {
    pub area: &'a str,
    pub page: &'a str,
    pub kind: &'a str,
    pub r#ref: &'a DraftRef,
    pub label: &'a str,
    pub by_sub: &'a str,
}

/// Record one write in the job's ledger (fixture `recordLedger`, §2.3 step 5). `before` = the value the write
/// replaced. If the draft is now the live value again, no approved cut bundles the key and the live release covers the
/// place kind (an uncovered kind with a draft value is kept: [`live_uncovered`]), the row is dropped (`None`);
/// otherwise it is upserted on `(tenant_id, job_id, key)` keeping its `id`, `before` and `live_moved`.
pub(crate) async fn record(
    c: &mut PgConnection,
    reg: &Registry,
    tenant: Uuid,
    job: &JobRef,
    e: &Entry<'_>,
    before: &Snap,
) -> ApiResult<Option<LedgerRow>> {
    let t = reg.target(e.kind)?;
    let key = t.key(e.r#ref);
    let at = Place {
        tenant,
        job: Some(job),
    };
    let after = t.get(&mut *c, at, e.r#ref).await?;
    let live = value(&mut *c, reg, job, &DesignOf::Live, e.kind, e.r#ref).await?;
    // a kind the live release does not store reads live = the working rows (§2.5), so `same` holds for every value
    // there; a draft value at such a place is still unpublished (publishing makes it part of the release): keep it
    let uncovered = after.is_some()
        && live_uncovered(&mut *c, reg, job, e.kind, e.r#ref)
            .await?
            .is_some();
    if same(t, &live, &after) && !uncovered && !bundled_in_approved(&mut *c, job, &key).await? {
        sqlx::query("DELETE FROM draft_ledger WHERE job_id = $1 AND key = $2")
            .bind(job.id)
            .bind(&key)
            .execute(&mut *c)
            .await?;
        return Ok(None);
    }
    let label: String = e.label.chars().take(MAX_LABEL_CHARS).collect();
    let row = sqlx::query_as::<_, Stored>(sqlx::AssertSqlSafe(format!(
        "INSERT INTO draft_ledger (tenant_id, job_id, key, kind, ref, area, page, label, before, after, by_sub) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) \
         ON CONFLICT (tenant_id, job_id, key) DO UPDATE \
            SET kind = EXCLUDED.kind, ref = EXCLUDED.ref, area = EXCLUDED.area, page = EXCLUDED.page, \
                label = EXCLUDED.label, after = EXCLUDED.after, at = clock_timestamp(), by_sub = EXCLUDED.by_sub \
         RETURNING {COLUMNS}"
    )))
    .bind(tenant)
    .bind(job.id)
    .bind(&key)
    .bind(e.kind)
    .bind(Value::Object(e.r#ref.clone()))
    .bind(e.area)
    .bind(e.page)
    .bind(&label)
    .bind(snap_to_json(before))
    .bind(snap_to_json(&after))
    .bind(e.by_sub)
    .fetch_one(&mut *c)
    .await?;
    row.row().map(Some)
}
