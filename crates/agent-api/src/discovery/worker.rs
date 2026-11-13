//! The import worker (T14a; plan "Global Constraints": no GPU from schedules). A tokio task spawned by the upload
//! request, and resumed at api boot ([`resume`]) for imports still `classifying` — it never creates work itself.
//!
//! It reads the import's unclassified rows in batches of [`BATCH`] (first-seen order), hands each batch to the
//! [`Classifier`], writes the buckets and advances `done`. Before every batch it re-reads the import: a row that is
//! gone (deleted, T15a `deleteDiscoveryImport`) or no longer `classifying` is the cancel flag, and the worker stops
//! ([`Outcome::Cancelled`]; an import a concurrent worker already finished is [`Outcome::Classified`]); its writes are conditional on the row, so a delete racing a batch leaves nothing behind.
//! When no row is left it computes `coverage` (covered weight / total), `long_tail` (unclear weight) and `volumes`
//! (per covered or hidden activity) and sets the import `classified`. A database error sets it `failed`.
//!
//! Leases (0190, [`crate::lease`]): spawned and resumed workers claim the import first and renew it before every batch,
//! so two replicas never run the same import ([`Outcome::HeldElsewhere`]); [`run`] is the unleased runner.
//!
//! The classifier is [`classifier`]'s (T14b, crate::discovery::classify: Laya over the job's effective activities).
//! It answers `None` when it cannot serve at all (the tenant has no `gates` provider): the import stays `classifying`
//! with its rows stored ([`Outcome::Parked`]) and the next sweep / boot resumes it once a provider is bound. A row the
//! classifier does not answer for is `unclear` (fail open, `laya_error_fails_open_to_unclear`). When no row is left
//! the uncovered ones are clustered ([`Classifier::cluster`], one LLM call; a failure leaves the import classified
//! with no clusters) before the import is finished.
use std::collections::HashMap;
use std::sync::Arc;

use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use sqlx::{PgPool, Postgres, Transaction};
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::error::ApiResult;
use crate::state::AppState;
use crate::tenancy;

/// Rows per classifier call.
pub const BATCH: i64 = 64;
/// Uncovered rows handed to the cluster drafter (heaviest first).
pub const MAX_CLUSTER_ROWS: i64 = 200;

/// The import a worker runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportRef {
    pub tenant: Uuid,
    pub job_id: Uuid,
    /// `imp-NNNN`.
    pub id: String,
}

/// One unclassified row.
#[derive(Clone, Debug)]
pub struct Pending {
    pub text_hash: String,
    /// The scrubbed excerpt (never raw text).
    pub excerpt: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bucket {
    /// An enabled activity of the job.
    Covered,
    /// A hidden template activity.
    Hidden,
    /// No activity of the template.
    Uncovered,
    /// Below the margin, or the classifier failed for it.
    Unclear,
    /// B8 (out of scope).
    OutOfScope,
}

impl Bucket {
    pub fn as_str(self) -> &'static str {
        match self {
            Bucket::Covered => "covered",
            Bucket::Hidden => "hidden",
            Bucket::Uncovered => "uncovered",
            Bucket::Unclear => "unclear",
            Bucket::OutOfScope => "out_of_scope",
        }
    }
}

/// The classifier's answer for one row.
#[derive(Clone, Debug)]
pub struct Classified {
    pub text_hash: String,
    pub bucket: Bucket,
    pub stage: Option<String>,
    /// `stage:activity` when known.
    pub activity: Option<String>,
    pub confidence: Option<f32>,
    /// false = the excerpt is dropped (a B8 / out-of-scope row keeps no text).
    pub keep_excerpt: bool,
}

/// One uncovered row handed to the cluster drafter.
#[derive(Clone, Debug)]
pub struct Uncovered {
    pub text_hash: String,
    /// The scrubbed excerpt.
    pub excerpt: String,
    pub weight: i32,
}

/// One drafted cluster of uncovered rows.
#[derive(Clone, Debug)]
pub struct ClusterDraft {
    pub label: String,
    /// The stored `suggestion` (`{kind: "tenant_extension", stage, slug, label, ..}`).
    pub suggestion: Value,
    /// `text_hash` of the member rows (each row in at most one cluster).
    pub members: Vec<String>,
    /// The drafting call's tokens, carried by one cluster of the call so the import's sum is the call's.
    pub drafted_tokens: i32,
}

/// Classifies a batch of an import's rows. `None` = no classifier is available: the worker parks the import.
pub trait Classifier: Send + Sync {
    fn classify<'a>(
        &'a self,
        at: &'a ImportRef,
        rows: &'a [Pending],
    ) -> BoxFuture<'a, Option<Vec<Classified>>>;

    /// Group the import's uncovered rows into clusters. No clusters (the default, or a drafting failure the
    /// classifier logged) is never an error: the import finishes classified.
    fn cluster<'a>(
        &'a self,
        _at: &'a ImportRef,
        _rows: &'a [Uncovered],
    ) -> BoxFuture<'a, Vec<ClusterDraft>> {
        Box::pin(async { Vec::new() })
    }
}

/// A classifier that serves nothing: every import parks in `classifying` (tests of the parking path).
#[derive(Clone, Copy, Debug, Default)]
pub struct Unwired;

impl Classifier for Unwired {
    fn classify<'a>(
        &'a self,
        _at: &'a ImportRef,
        _rows: &'a [Pending],
    ) -> BoxFuture<'a, Option<Vec<Classified>>> {
        Box::pin(async { None })
    }
}

/// The classifier uploads, the boot resume and the lease sweep use: Laya over the job's effective activities, the
/// tenant's `gates` provider (crate::discovery::classify).
pub fn classifier(st: &AppState) -> Arc<dyn Classifier> {
    Arc::new(super::classify::LlmClassifier::new(st.clone()))
}

/// How a worker run ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Every row classified; the import is `classified` (by this worker or a concurrent one on the same import).
    Classified,
    /// The classifier is not available; the import stays `classifying`.
    Parked,
    /// The import was deleted (or is no longer `classifying` and not `classified`, e.g. `failed`).
    Cancelled,
    /// A database error; the import is `failed`.
    Failed,
    /// Another owner holds the import's lease (the claim was refused), or this one lost it mid-run: nothing (more)
    /// was done here.
    HeldElsewhere,
}

/// A transaction scoped to the import's tenant and job.
pub(super) async fn begin(
    db: &PgPool,
    at: &ImportRef,
) -> ApiResult<Transaction<'static, Postgres>> {
    let mut tx = tenancy::begin_scoped(db, at.tenant).await?;
    sqlx::query("SELECT set_config('app.current_job', $1, true)")
        .bind(at.job_id.to_string())
        .execute(&mut *tx)
        .await?;
    Ok(tx)
}

/// Spawn [`run_leased`] (as this process, [`crate::lease::owner`]) on the runtime.
pub fn spawn(db: PgPool, c: Arc<dyn Classifier>, at: ImportRef) -> JoinHandle<Outcome> {
    tokio::spawn(async move { run_leased(&db, c.as_ref(), &at, crate::lease::owner()).await })
}

/// Boot and sweep: spawn a worker for every import still `classifying` that no live lease holds (every tenant, via
/// the definer `discovery_classifying_imports()`), as this process.
pub async fn resume(db: &PgPool, c: Arc<dyn Classifier>) -> ApiResult<Vec<JoinHandle<Outcome>>> {
    resume_as(db, c, crate::lease::owner()).await
}

/// [`resume`] as `owner`: each spawned worker claims its import under that owner.
pub async fn resume_as(
    db: &PgPool,
    c: Arc<dyn Classifier>,
    owner: &str,
) -> ApiResult<Vec<JoinHandle<Outcome>>> {
    let open: Vec<(Uuid, Uuid, String)> =
        sqlx::query_as("SELECT tenant_id, job_id, id FROM discovery_classifying_imports()")
            .fetch_all(db)
            .await?;
    Ok(open
        .into_iter()
        .map(|(tenant, job_id, id)| {
            let (db, c, owner) = (db.clone(), Arc::clone(&c), owner.to_string());
            let at = ImportRef { tenant, job_id, id };
            tokio::spawn(async move { run_leased(&db, c.as_ref(), &at, &owner).await })
        })
        .collect())
}

/// [`run`] under a lease: claim the import as `owner` first (refused → [`Outcome::HeldElsewhere`], nothing done),
/// renew it before every batch and always release it at the end.
pub async fn run_leased(db: &PgPool, c: &dyn Classifier, at: &ImportRef, owner: &str) -> Outcome {
    let granted: Result<bool, sqlx::Error> =
        sqlx::query_scalar("SELECT discovery_import_claim($1, $2, $3)")
            .bind(&at.id)
            .bind(owner)
            .bind(crate::lease::TTL_SECS)
            .fetch_one(db)
            .await;
    match granted {
        Ok(true) => {}
        Ok(false) => return Outcome::HeldElsewhere,
        Err(e) => {
            tracing::warn!(import = %at.id, error = %e, "discovery_import_claim_failed");
            return Outcome::HeldElsewhere;
        }
    }
    let out = run_as(db, c, at, Some(owner)).await;
    if let Err(e) = sqlx::query("SELECT discovery_import_release($1, $2)")
        .bind(&at.id)
        .bind(owner)
        .execute(db)
        .await
    {
        tracing::warn!(import = %at.id, error = %e, "discovery_import_release_failed");
    }
    out
}

/// Run the import to its end (module docs), without a lease: the caller guarantees nothing else runs it.
pub async fn run(db: &PgPool, c: &dyn Classifier, at: &ImportRef) -> Outcome {
    run_as(db, c, at, None).await
}

async fn run_as(db: &PgPool, c: &dyn Classifier, at: &ImportRef, owner: Option<&str>) -> Outcome {
    match run_batches(db, c, at, owner).await {
        Ok(o) => o,
        Err(e) => {
            tracing::warn!(import = %at.id, error = %e, "discovery_import_failed");
            if let Err(e) = fail(db, at, "worker_error").await {
                tracing::warn!(import = %at.id, error = %e, "discovery_import_fail_mark_failed");
            }
            Outcome::Failed
        }
    }
}

async fn run_batches(
    db: &PgPool,
    c: &dyn Classifier,
    at: &ImportRef,
    owner: Option<&str>,
) -> ApiResult<Outcome> {
    loop {
        let mut tx = begin(db, at).await?;
        let status: Option<String> =
            sqlx::query_scalar("SELECT status FROM discovery_import WHERE id = $1")
                .bind(&at.id)
                .fetch_optional(&mut *tx)
                .await?;
        match status.as_deref() {
            Some("classifying") => {}
            // another worker on the same import (the upload's own, a boot resume, another replica) classified the
            // last batch and finished it between two of ours: the import ended classified, not cancelled
            Some("classified") => {
                tx.commit().await?;
                return Ok(Outcome::Classified);
            }
            _ => {
                tx.commit().await?;
                return Ok(Outcome::Cancelled);
            }
        }
        if let Some(owner) = owner {
            let held: bool = sqlx::query_scalar("SELECT discovery_import_renew($1, $2, $3)")
                .bind(&at.id)
                .bind(owner)
                .bind(crate::lease::TTL_SECS)
                .fetch_one(&mut *tx)
                .await?;
            if !held {
                tx.commit().await?;
                return Ok(Outcome::HeldElsewhere);
            }
        }
        let batch: Vec<(String, Option<String>)> = sqlx::query_as(
            "SELECT text_hash, excerpt FROM discovery_row WHERE import_id = $1 AND bucket IS NULL \
             ORDER BY ord LIMIT $2",
        )
        .bind(&at.id)
        .bind(BATCH)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        if batch.is_empty() {
            cluster_uncovered(db, c, at).await?;
            finish(db, at).await?;
            return Ok(Outcome::Classified);
        }
        let pending: Vec<Pending> = batch
            .into_iter()
            .map(|(text_hash, excerpt)| Pending { text_hash, excerpt })
            .collect();
        let Some(got) = c.classify(at, &pending).await else {
            return Ok(Outcome::Parked);
        };
        write_batch(db, at, &pending, &got).await?;
    }
}

/// The batch's buckets (a row without an answer → `unclear`) and the import's `done`; both no-ops once the import
/// is gone.
async fn write_batch(
    db: &PgPool,
    at: &ImportRef,
    pending: &[Pending],
    got: &[Classified],
) -> ApiResult<()> {
    let mut by: HashMap<&str, &Classified> =
        got.iter().map(|g| (g.text_hash.as_str(), g)).collect();
    let mut hashes: Vec<String> = Vec::with_capacity(pending.len());
    let mut buckets: Vec<String> = Vec::with_capacity(pending.len());
    let mut stages: Vec<Option<String>> = Vec::with_capacity(pending.len());
    let mut activities: Vec<Option<String>> = Vec::with_capacity(pending.len());
    let mut confidences: Vec<Option<f32>> = Vec::with_capacity(pending.len());
    let mut keep: Vec<bool> = Vec::with_capacity(pending.len());
    for p in pending {
        hashes.push(p.text_hash.clone());
        match by.remove(p.text_hash.as_str()) {
            Some(g) => {
                buckets.push(g.bucket.as_str().to_string());
                stages.push(g.stage.clone());
                activities.push(g.activity.clone());
                confidences.push(g.confidence.map(|f| f.clamp(0.0, 1.0)));
                keep.push(g.keep_excerpt);
            }
            None => {
                buckets.push(Bucket::Unclear.as_str().to_string());
                stages.push(None);
                activities.push(None);
                confidences.push(None);
                keep.push(true);
            }
        }
    }
    let mut tx = begin(db, at).await?;
    sqlx::query(
        "UPDATE discovery_row r SET bucket = u.bucket, stage = u.stage, activity = u.activity, \
                confidence = u.confidence, excerpt = CASE WHEN u.keep THEN r.excerpt ELSE NULL END \
           FROM UNNEST($2::text[], $3::text[], $4::text[], $5::text[], $6::real[], $7::bool[]) \
                AS u(text_hash, bucket, stage, activity, confidence, keep) \
          WHERE r.import_id = $1 AND r.text_hash = u.text_hash AND r.bucket IS NULL",
    )
    .bind(&at.id)
    .bind(&hashes)
    .bind(&buckets)
    .bind(&stages)
    .bind(&activities)
    .bind(&confidences)
    .bind(&keep)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE discovery_import SET done = (SELECT count(*) FROM discovery_row \
                                              WHERE import_id = $1 AND bucket IS NOT NULL)::integer \
          WHERE id = $1 AND status = 'classifying'",
    )
    .bind(&at.id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Cluster the import's uncovered rows (module docs): nothing when the import already has clusters (a resumed run)
/// or no row is uncovered. The classifier's failure is no clusters, not an error.
async fn cluster_uncovered(db: &PgPool, c: &dyn Classifier, at: &ImportRef) -> ApiResult<()> {
    let mut tx = begin(db, at).await?;
    let have: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM discovery_cluster WHERE import_id = $1)")
            .bind(&at.id)
            .fetch_one(&mut *tx)
            .await?;
    let rows: Vec<(String, String, i32)> = if have {
        Vec::new()
    } else {
        sqlx::query_as(
            "SELECT text_hash, excerpt, weight FROM discovery_row \
              WHERE import_id = $1 AND bucket = 'uncovered' AND excerpt IS NOT NULL \
              ORDER BY weight DESC, ord LIMIT $2",
        )
        .bind(&at.id)
        .bind(MAX_CLUSTER_ROWS)
        .fetch_all(&mut *tx)
        .await?
    };
    tx.commit().await?;
    if rows.is_empty() {
        return Ok(());
    }
    let uncovered: Vec<Uncovered> = rows
        .into_iter()
        .map(|(text_hash, excerpt, weight)| Uncovered {
            text_hash,
            excerpt,
            weight,
        })
        .collect();
    let drafts = c.cluster(at, &uncovered).await;
    if drafts.is_empty() {
        return Ok(());
    }
    write_clusters(db, at, &uncovered, &drafts).await
}

/// Store the drafts as clusters `<import>-c<n>` (tickets = the members' weight, examples = the heaviest three
/// excerpts) and mark their rows. A no-op once the import is gone or no longer `classifying`; a hash the drafter
/// invented, or a row already in an earlier cluster, is dropped.
async fn write_clusters(
    db: &PgPool,
    at: &ImportRef,
    rows: &[Uncovered],
    drafts: &[ClusterDraft],
) -> ApiResult<()> {
    let by: HashMap<&str, &Uncovered> = rows.iter().map(|r| (r.text_hash.as_str(), r)).collect();
    let mut taken: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut tx = begin(db, at).await?;
    let live: Option<bool> = sqlx::query_scalar(
        "SELECT true FROM discovery_import WHERE id = $1 AND status = 'classifying'",
    )
    .bind(&at.id)
    .fetch_optional(&mut *tx)
    .await?;
    if live.is_none() {
        tx.commit().await?;
        return Ok(());
    }
    for (i, d) in drafts.iter().enumerate() {
        let mut members: Vec<&Uncovered> = d
            .members
            .iter()
            .filter_map(|h| by.get(h.as_str()).copied())
            .filter(|m| taken.insert(m.text_hash.as_str()))
            .collect();
        if members.is_empty() {
            continue;
        }
        members.sort_by_key(|m| std::cmp::Reverse(m.weight));
        let tickets: i64 = members.iter().map(|m| i64::from(m.weight)).sum();
        let examples: Vec<&str> = members.iter().take(3).map(|m| m.excerpt.as_str()).collect();
        let id = format!("{}-c{}", at.id, i + 1);
        sqlx::query(
            "INSERT INTO discovery_cluster (import_id, id, label, tickets, examples, suggestion, drafted_tokens) \
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(&at.id)
        .bind(&id)
        .bind(&d.label)
        .bind(i32::try_from(tickets).unwrap_or(i32::MAX))
        .bind(json!(examples))
        .bind(&d.suggestion)
        .bind(d.drafted_tokens.max(0))
        .execute(&mut *tx)
        .await?;
        let hashes: Vec<&str> = members.iter().map(|m| m.text_hash.as_str()).collect();
        sqlx::query(
            "UPDATE discovery_row SET cluster_id = $3 WHERE import_id = $1 AND text_hash = ANY($2::text[])",
        )
        .bind(&at.id)
        .bind(&hashes)
        .bind(&id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Every row classified: coverage, long tail, volumes; status `classified`.
async fn finish(db: &PgPool, at: &ImportRef) -> ApiResult<()> {
    let mut tx = begin(db, at).await?;
    sqlx::query(FINISH).bind(&at.id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

/// `volumes[]` = `{activity, label, stage, tickets, feasible, usable, enabled}` per covered/hidden activity, most
/// tickets first (label = the key and `feasible` 0 / `usable` null until T5 / T14b read the job's journey and B7/B4).
const FINISH: &str = "
WITH r AS (SELECT bucket, stage, activity, weight::bigint AS weight FROM discovery_row WHERE import_id = $1),
     tot AS (SELECT coalesce(sum(weight), 0) AS w FROM r),
     vol AS (SELECT activity, min(stage) AS stage, sum(weight) AS tickets, bool_or(bucket = 'covered') AS enabled
               FROM r WHERE activity IS NOT NULL AND bucket IN ('covered', 'hidden') GROUP BY activity)
UPDATE discovery_import
   SET status = 'classified',
       done = distinct_rows,
       coverage = CASE WHEN (SELECT w FROM tot) = 0 THEN NULL
                       ELSE ((SELECT coalesce(sum(weight), 0) FROM r WHERE bucket = 'covered')::double precision
                             / (SELECT w FROM tot)::double precision)::real END,
       long_tail = least((SELECT coalesce(sum(weight), 0) FROM r WHERE bucket = 'unclear'), 2147483647)::integer,
       volumes = coalesce((SELECT jsonb_agg(jsonb_build_object('activity', activity, 'label', activity,
                                                               'stage', stage, 'tickets', tickets, 'feasible', 0,
                                                               'usable', NULL, 'enabled', enabled)
                                            ORDER BY tickets DESC, activity)
                             FROM vol), '[]'::jsonb)
 WHERE id = $1 AND status = 'classifying'";

/// Mark the import `failed` with `code` (no text of any row in it).
async fn fail(db: &PgPool, at: &ImportRef, code: &str) -> ApiResult<()> {
    let mut tx = begin(db, at).await?;
    sqlx::query(
        "UPDATE discovery_import SET status = 'failed', error = $2 WHERE id = $1 AND status = 'classifying'",
    )
    .bind(&at.id)
    .bind(code)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}
