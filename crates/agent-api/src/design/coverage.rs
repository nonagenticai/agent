//! Coverage (spec B2 §2.5, D68):
//!
//! - [`language_level`]: knowledge coverage per language, the hook B2 T7 reads for
//!   `JobLanguagesView.options[].knowledge`. B3 implements it (fixture `languageLevel` over `coverageOf`: the job's
//!   required knowledge kinds × the documents in the kinds' bound stores; `crate::knowledge::coverage`).
//! - [`backfill`]: snapshot coverage of late-registered ledger kinds (§2.5 "late registration"). A release cut, or a
//!   baseline taken, before kind K was registered stores no K rows, and its `snapshot_kinds` (`jobs.baseline_kinds`)
//!   lacks K. At boot, after migrations and before serving, every such release and baseline gets K's places copied
//!   from the job's **current working rows** (`ON CONFLICT DO NOTHING`) and K appended to its kinds (append-only,
//!   trigger `job_release_immutable`). Rationale: before K's plan landed, K had no draft semantics, so its stored
//!   value *was* what ran.
//!
//! The copy takes the working rows as they are NOW, drafts included. While a kind was uncovered, a draft value at
//! one of its places stayed in the draft ledger although it equalled live (`ledger::record`: live fell through to the
//! working rows, so Production already ran it). Once the copy covers the kind that value IS the release's, so
//! [`settle_ledger`] drops those rows in the same transaction (the `ledger::record` rule: draft = live, covered, not
//! bundled in an approved cut). Without it a boot backfill that failed, then an edit of kind K, then a later
//! backfill left `live == draft` with the row, `unpublished = 1`, and nothing a publish could clear.
//!
//! [`backfill`] runs per tenant as `agent_app` (one transaction per tenant, RLS-scoped), each transaction under
//! `pg_advisory_xact_lock(hashtext('agent.design.coverage'))`, so replicas booting at once serialise: the second
//! finds nothing left to do. Each job is locked `FOR UPDATE` (the lock `design::track` takes) so a tracked write on
//! another, already serving replica cannot interleave with the copy.
//!
//! The backfill never stops a replica from starting. Each tenant transaction waits at most
//! [`BackfillOptions::lock_wait`] for the coverage lock and [`BackfillOptions::lock_timeout`] for any other lock (a
//! job row held by a serving replica); it is retried on 40P01 / 40001 / 55P03, and a tenant that still fails is
//! logged (`design_coverage_backfill_failed`, error level, with the tenant and the error), counted in
//! [`Backfilled::failed_tenants`] and skipped. Reads stay safe meanwhile (an uncovered kind falls through to the
//! working rows) and the next boot retries it.
//!
//! Releases that store no snapshot (`snapshot_kinds = '{}'`, unappliable: 422 `bundle_invalid`) are left alone, so
//! they stay unappliable.
//!
//! Known gap (rolling deploy): a cut or baseline made by an OLD replica after this replica's backfill lacks the new
//! kinds until the next boot; reads of those kinds for it fall through to the working rows meanwhile.
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;
use sqlx::{PgConnection, PgPool, Postgres, Transaction};
use uuid::Uuid;

use super::ledger;
use super::read::{live_uncovered, same, value};
use super::snapshot::working_rows;
use super::{DesignOf, DesignRow, JobRef, Place, Registry, snap_to_json};
use crate::error::{ApiError, ApiResult};
use crate::platform_db;
use crate::tenancy::begin_scoped;

/// What the knowledge base covers for one language (fixture `'full' | 'partial' | 'none'`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    /// Every required knowledge kind has a document in the language.
    Full,
    /// Some required kinds do.
    Partial,
    /// None do (or no coverage is registered yet).
    None,
}

/// The job's knowledge coverage in `lang` (fixture `languageLevel(coverage, lang)`), from B3's
/// [`crate::knowledge::coverage::language_level`] (local state only; R10 pack-only cells are `Partial`).
pub async fn language_level(c: &mut PgConnection, job: &JobRef, lang: &str) -> ApiResult<Level> {
    crate::knowledge::coverage::language_level(c, job, lang).await
}

/// The advisory lock text: [`backfill`] holds `pg_advisory_xact_lock(hashtext(COVERAGE_LOCK))` (the one-argument,
/// bigint lock space) for each tenant's transaction.
pub const COVERAGE_LOCK: &str = "agent.design.coverage";

/// What one [`backfill`] run changed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Backfilled {
    /// Releases whose `snapshot_kinds` grew.
    pub releases: u64,
    /// Baselines (`jobs.baseline_kinds`) that grew.
    pub baselines: u64,
    /// `release_snapshot` + `release_baseline` rows inserted.
    pub rows: u64,
    /// Tenants whose backfill failed after its retries (logged, rolled back, retried at the next boot).
    pub failed_tenants: u64,
}

impl Backfilled {
    fn add(&mut self, o: Backfilled) {
        self.releases += o.releases;
        self.baselines += o.baselines;
        self.rows += o.rows;
        self.failed_tenants += o.failed_tenants;
    }
}

/// How hard [`backfill`] tries per tenant. [`Default`] is what boot uses; tests shrink the timeouts.
#[doc(hidden)]
#[derive(Clone, Copy, Debug)]
pub struct BackfillOptions {
    /// `lock_timeout` while waiting for the coverage lock (another replica's backfill of the same tenant).
    pub lock_wait: Duration,
    /// `lock_timeout` for every lock after it (job rows a serving replica may hold).
    pub lock_timeout: Duration,
    /// Tries per tenant (retrying 40P01 / 40001 / 55P03 only).
    pub attempts: u32,
    /// Sleep before try `n + 1` is `backoff * n`.
    pub backoff: Duration,
}

impl Default for BackfillOptions {
    fn default() -> Self {
        BackfillOptions {
            lock_wait: Duration::from_secs(60),
            lock_timeout: Duration::from_secs(5),
            attempts: 3,
            backoff: Duration::from_millis(200),
        }
    }
}

/// A deadlock, serialization failure or lock timeout: worth another try.
fn retryable(e: &ApiError) -> bool {
    let ApiError::Internal(a) = e else {
        return false;
    };
    a.downcast_ref::<sqlx::Error>()
        .and_then(|s| s.as_database_error())
        .and_then(|d| d.code())
        .is_some_and(|c| matches!(c.as_ref(), "40P01" | "40001" | "55P03"))
}

/// `SET LOCAL lock_timeout` (`set_config(…, true)`, so the value can be bound; the SQL stays a literal).
async fn set_lock_timeout(tx: &mut Transaction<'static, Postgres>, d: Duration) -> ApiResult<()> {
    sqlx::query("SELECT set_config('lock_timeout', $1, true)")
        .bind(format!("{}ms", d.as_millis().max(1)))
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Backfill every stored release snapshot and baseline of every tenant with the registered snapshot kinds it does
/// not cover (see the module doc). Idempotent. `db` is the runtime pool (`agent_app`); the tenant list is read as
/// `agent_platform` (`agent_list_tenants()` is granted to it alone, 0031). Never `Err`: a failing tenant is logged,
/// counted in [`Backfilled::failed_tenants`] and skipped.
pub async fn backfill(db: &PgPool, reg: &Registry) -> ApiResult<Backfilled> {
    backfill_with(db, reg, &BackfillOptions::default()).await
}

/// [`backfill`] with explicit [`BackfillOptions`].
#[doc(hidden)]
pub async fn backfill_with(
    db: &PgPool,
    reg: &Registry,
    opts: &BackfillOptions,
) -> ApiResult<Backfilled> {
    let kinds = reg.snapshot_kinds();
    let mut out = Backfilled::default();
    if kinds.is_empty() {
        return Ok(out);
    }
    let tenants = match list_tenants(db).await {
        Ok(t) => t,
        Err(e) => {
            tracing::error!(error = %e, "design_coverage_backfill_tenants_unreadable");
            return Ok(out);
        }
    };
    let attempts = opts.attempts.max(1);
    for tenant in tenants {
        let mut attempt = 1;
        loop {
            match backfill_tenant_tx(db, reg, tenant, &kinds, opts).await {
                Ok(done) => {
                    out.add(done);
                    break;
                }
                Err(e) if attempt < attempts && retryable(&e) => {
                    tracing::warn!(tenant = %tenant, attempt, error = %e, "design_coverage_backfill_retry");
                    tokio::time::sleep(opts.backoff * attempt).await;
                    attempt += 1;
                }
                Err(e) => {
                    tracing::error!(
                        tenant = %tenant,
                        attempts = attempt,
                        error = %e,
                        "design_coverage_backfill_failed"
                    );
                    out.failed_tenants += 1;
                    break;
                }
            }
        }
    }
    tracing::info!(
        releases = out.releases,
        baselines = out.baselines,
        rows = out.rows,
        failed_tenants = out.failed_tenants,
        "design_coverage_backfill"
    );
    Ok(out)
}

async fn list_tenants(db: &PgPool) -> ApiResult<Vec<Uuid>> {
    let mut tx = platform_db::begin_platform(db).await?;
    let tenants: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM agent_list_tenants()")
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(tenants)
}

/// One try at one tenant: its own transaction, rolled back (on drop) when anything fails.
async fn backfill_tenant_tx(
    db: &PgPool,
    reg: &Registry,
    tenant: Uuid,
    kinds: &[&'static str],
    opts: &BackfillOptions,
) -> ApiResult<Backfilled> {
    let mut tx = begin_scoped(db, tenant).await?;
    set_lock_timeout(&mut tx, opts.lock_wait).await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1))")
        .bind(COVERAGE_LOCK)
        .execute(&mut *tx)
        .await?;
    set_lock_timeout(&mut tx, opts.lock_timeout).await?;
    let done = backfill_tenant(&mut tx, reg, tenant, kinds).await?;
    tx.commit().await?;
    Ok(done)
}

/// The kinds of `kinds` (registry order) that `have` lacks.
fn lacking(kinds: &[&'static str], have: &[String]) -> Vec<&'static str> {
    kinds
        .iter()
        .copied()
        .filter(|k| !have.iter().any(|h| h == k))
        .collect()
}

fn names(kinds: &[&'static str]) -> Vec<String> {
    kinds.iter().map(|k| k.to_string()).collect()
}

/// One tenant, in its RLS-scoped transaction, under the coverage lock.
async fn backfill_tenant(
    c: &mut PgConnection,
    reg: &Registry,
    tenant: Uuid,
    kinds: &[&'static str],
) -> ApiResult<Backfilled> {
    let mut out = Backfilled::default();
    let jobs: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT j.id, j.key FROM jobs j \
          WHERE (j.baseline_kinds IS NOT NULL AND NOT j.baseline_kinds @> $1::text[]) \
             OR EXISTS (SELECT 1 FROM job_release r \
                         WHERE r.job_id = j.id AND cardinality(r.snapshot_kinds) > 0 \
                           AND NOT r.snapshot_kinds @> $1::text[]) \
          ORDER BY j.id",
    )
    .bind(names(kinds))
    .fetch_all(&mut *c)
    .await?;
    for (id, key) in jobs {
        let job = JobRef { id, key };
        // Lock the job (as `design::track` does) and re-read its coverage under the lock.
        let Some(baseline_kinds): Option<Option<Vec<String>>> =
            sqlx::query_scalar("SELECT baseline_kinds FROM jobs WHERE id = $1 FOR UPDATE")
                .bind(job.id)
                .fetch_optional(&mut *c)
                .await?
        else {
            continue;
        };
        let releases: Vec<(String, Vec<String>)> = sqlx::query_as(
            "SELECT release_id, snapshot_kinds FROM job_release \
              WHERE job_id = $1 AND cardinality(snapshot_kinds) > 0 ORDER BY release_id",
        )
        .bind(job.id)
        .fetch_all(&mut *c)
        .await?;
        let baseline_missing = baseline_kinds
            .as_deref()
            .map(|have| lacking(kinds, have))
            .unwrap_or_default();
        let release_missing: Vec<(String, Vec<&'static str>)> = releases
            .into_iter()
            .map(|(release, have)| (release, lacking(kinds, &have)))
            .filter(|(_, m)| !m.is_empty())
            .collect();
        let wanted: Vec<&'static str> = kinds
            .iter()
            .copied()
            .filter(|k| {
                baseline_missing.contains(k) || release_missing.iter().any(|(_, m)| m.contains(k))
            })
            .collect();
        if wanted.is_empty() {
            continue;
        }
        let rows = working_rows(&mut *c, reg, tenant, &job, &wanted).await?;
        if !baseline_missing.is_empty() {
            for r in of_kinds(&rows, &baseline_missing) {
                out.rows += sqlx::query(
                    "INSERT INTO release_baseline (tenant_id, job_id, key, kind, ref, snap) \
                     VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (tenant_id, job_id, key) DO NOTHING",
                )
                .bind(tenant)
                .bind(job.id)
                .bind(&r.key)
                .bind(&r.kind)
                .bind(Value::Object(r.r#ref.clone()))
                .bind(snap_to_json(&r.snap))
                .execute(&mut *c)
                .await?
                .rows_affected();
            }
            sqlx::query(
                "UPDATE jobs SET baseline_kinds = baseline_kinds || $2::text[] WHERE id = $1",
            )
            .bind(job.id)
            .bind(names(&baseline_missing))
            .execute(&mut *c)
            .await?;
            out.baselines += 1;
        }
        for (release, missing) in &release_missing {
            for r in of_kinds(&rows, missing) {
                out.rows += sqlx::query(
                    "INSERT INTO release_snapshot (tenant_id, job_id, release_id, key, kind, ref, snap) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7) \
                     ON CONFLICT (tenant_id, job_id, release_id, key) DO NOTHING",
                )
                .bind(tenant)
                .bind(job.id)
                .bind(release)
                .bind(&r.key)
                .bind(&r.kind)
                .bind(Value::Object(r.r#ref.clone()))
                .bind(snap_to_json(&r.snap))
                .execute(&mut *c)
                .await?
                .rows_affected();
            }
            sqlx::query(
                "UPDATE job_release SET snapshot_kinds = snapshot_kinds || $3::text[] \
                  WHERE job_id = $1 AND release_id = $2",
            )
            .bind(job.id)
            .bind(release)
            .bind(names(missing))
            .execute(&mut *c)
            .await?;
            out.releases += 1;
        }
        settle_ledger(&mut *c, reg, tenant, &job).await?;
    }
    Ok(out)
}

/// Drop the job's ledger rows the backfill made redundant: draft = live, the live release now covers the place's
/// kind and no approved cut bundles the key (the `ledger::record` rule). Rows of any other state are untouched.
async fn settle_ledger(
    c: &mut PgConnection,
    reg: &Registry,
    tenant: Uuid,
    job: &JobRef,
) -> ApiResult<()> {
    for row in ledger::of_job(&mut *c, job).await? {
        let t = reg.target(&row.kind)?;
        let at = Place {
            tenant,
            job: Some(job),
        };
        let draft = t.get(&mut *c, at, &row.r#ref).await?;
        let live = value(&mut *c, reg, job, &DesignOf::Live, &row.kind, &row.r#ref).await?;
        if same(t, &live, &draft)
            && live_uncovered(&mut *c, reg, job, &row.kind, &row.r#ref)
                .await?
                .is_none()
            && !ledger::bundled_in_approved(&mut *c, job, &row.key).await?
        {
            sqlx::query("DELETE FROM draft_ledger WHERE job_id = $1 AND key = $2")
                .bind(job.id)
                .bind(&row.key)
                .execute(&mut *c)
                .await?;
        }
    }
    Ok(())
}

/// The rows of `rows` whose kind is one of `kinds`.
fn of_kinds<'a>(
    rows: &'a [DesignRow],
    kinds: &'a [&'static str],
) -> impl Iterator<Item = &'a DesignRow> + 'a {
    rows.iter()
        .filter(move |r| kinds.iter().any(|k| r.kind == *k))
}
