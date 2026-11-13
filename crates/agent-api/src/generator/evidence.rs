//! B8 T10a / F-53: ticket evidence uploaded to a PLATFORM run (`uploadGeneratorEvidence`). The file is parsed and
//! scrubbed with the discovery parser (`discovery::rows`: card, password and one-time-code rows are never stored;
//! excerpts are redacted and at most 1 KiB), stored as `generator_evidence` + `generator_evidence_row`, and one
//! background task classifies it: Laya (the platform-owned tenant's internal `gates` provider) answers, per row,
//! which of the run's journey activities the message asks for (accepted `activity` items, else the department's
//! grid), and one drafter call groups the uncovered rows into clusters. The clusters are rule 7's information
//! findings on the Validate step (`findings::evidence_findings`), never red.
//!
//! The task is caused by the upload request, resumed at boot ([`boot`]) and never created on its own. Every Laya
//! call and the cluster call is one `usage_event` (source `generator`, use `evidence_classify` / `evidence_cluster`)
//! on a connection scoped to the platform-owned tenant, attributed to the uploader. No `gates` provider: the upload
//! is refused with 409 `gates_provider_missing`, and an evidence parked by a vanished provider stays `classifying`.
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::multipart::MultipartError;
use axum::extract::{FromRequest, Multipart, Request};
use axum::http::StatusCode;
use futures_util::stream::{self, StreamExt};
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use super::llm::{Drafter, OpenAiDrafter};
use super::prompts::Call;
use super::run::RunRow;
use super::{drive, store};
use crate::audit::Actor;
use crate::design::{DesignOf, JobRef};
use crate::discovery::classify::{self as dclassify, Activity, NONE_KEY, QUESTION_KEY};
use crate::discovery::rows::{self, Collected, Format};
use crate::discovery::worker::{Bucket, Uncovered};
use crate::error::{ApiError, ApiResult};
use crate::job_design::gates::laya::{LayaClient, LayaError};
use crate::job_design::types::EffGateQuestion;
use crate::journey::grid;
use crate::platform_db::begin_platform;
use crate::providers::llm::{self, ResolvedLlm, Use};
use crate::state::AppState;
use crate::tenancy;
use crate::usage::{self, LlmUsage, UsageEvent};

/// Largest export an upload reads.
pub const MAX_UPLOAD_BYTES: usize = 20 * 1024 * 1024;
/// The route's body limit: the file plus its form fields.
pub const UPLOAD_BODY_LIMIT: usize = MAX_UPLOAD_BYTES + 64 * 1024;
const PARSE_TIMEOUT: Duration = Duration::from_secs(30);
const CONCURRENCY: usize = 8;
const BATCH: usize = 64;
const MAX_CLUSTER_ROWS: usize = 200;
/// The `usage_event.use` labels.
pub const USE_CLASSIFY: &str = "evidence_classify";
pub const USE_CLUSTER: &str = "evidence_cluster";

/// An uploaded export and the columns chosen for it.
pub struct Upload {
    pub file_name: String,
    pub format: Format,
    pub bytes: Vec<u8>,
    pub text_column: Option<String>,
    pub count_column: Option<String>,
}

fn bad_body(msg: &str) -> ApiError {
    ApiError::BadRequest("invalid_body", msg.to_string())
}

fn multipart_error(e: MultipartError) -> ApiError {
    if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
        rows::too_large()
    } else {
        bad_body("The multipart body is not valid.")
    }
}

fn column_field(v: &str) -> Option<String> {
    let t = v.trim();
    (!t.is_empty()).then(|| t.to_string())
}

/// Multipart `file` (`.csv`, `.json`, `.xlsx`; at most 20 MB), `text_column?`, `count_column?`; the format is refused
/// (422 `unsupported_file`) before the file is read.
pub async fn read_upload(req: Request) -> ApiResult<Upload> {
    let mut mp = Multipart::from_request(req, &())
        .await
        .map_err(|_| bad_body("The body must be multipart/form-data with a file field."))?;
    let mut file: Option<(String, Format, Vec<u8>)> = None;
    let (mut text_column, mut count_column) = (None, None);
    while let Some(mut field) = mp.next_field().await.map_err(multipart_error)? {
        match field.name().unwrap_or_default() {
            "file" if file.is_none() => {
                let file_name = field.file_name().unwrap_or_default().to_string();
                let format = Format::of(&file_name).ok_or_else(rows::unsupported_file)?;
                let mut bytes: Vec<u8> = Vec::new();
                while let Some(chunk) = field.chunk().await.map_err(multipart_error)? {
                    if bytes.len() + chunk.len() > MAX_UPLOAD_BYTES {
                        return Err(rows::too_large());
                    }
                    bytes.extend_from_slice(&chunk);
                }
                file = Some((file_name, format, bytes));
            }
            "text_column" => {
                text_column = column_field(&field.text().await.map_err(multipart_error)?);
            }
            "count_column" => {
                count_column = column_field(&field.text().await.map_err(multipart_error)?);
            }
            _ => {}
        }
    }
    let (file_name, format, bytes) =
        file.ok_or_else(|| bad_body("Attach the export in the file field."))?;
    Ok(Upload {
        file_name,
        format,
        bytes,
        text_column,
        count_column,
    })
}

/// Parse and scrub on the blocking pool (past 30 s: 422 `unsupported_file`).
pub async fn collect(up: Upload) -> ApiResult<(String, Format, Collected)> {
    let file = crate::discovery::import::file_label(&up.file_name);
    let format = up.format;
    let work = move || {
        let t = rows::parse(up.format, &up.bytes)?;
        rows::collect(&t, up.text_column.as_deref(), up.count_column.as_deref())
    };
    match tokio::time::timeout(PARSE_TIMEOUT, tokio::task::spawn_blocking(work)).await {
        Ok(Ok(r)) => Ok((file, format, r?)),
        Ok(Err(e)) => Err(ApiError::Internal(anyhow::anyhow!(
            "evidence parse task: {e}"
        ))),
        Err(_) => Err(ApiError::Unprocessable(
            "unsupported_file",
            "The file took too long to read; send a smaller or simpler export.".into(),
        )),
    }
}

/// 409 `gates_provider_missing` unless the platform-owned tenant has a `gates` provider that can serve. Returns it.
pub async fn gates_provider(st: &AppState, run: &RunRow) -> ApiResult<ResolvedLlm> {
    let missing = || {
        ApiError::Conflict(
            "gates_provider_missing",
            "The platform tenant has no gates provider bound: evidence cannot be classified."
                .into(),
        )
    };
    let tenant = st.cfg.platform_tenant_id.ok_or_else(missing)?;
    let mut tx = tenancy::begin_scoped(&st.db, tenant).await?;
    let job = JobRef {
        id: Uuid::nil(),
        key: run.job_key.clone(),
    };
    let r = llm::resolve(&mut tx, st, tenant, &job, Use::Gates, &DesignOf::Draft).await;
    tx.commit().await?;
    match r {
        Ok(Some(r)) => Ok(r),
        Ok(None) | Err(ApiError::Conflict(..)) => Err(missing()),
        Err(e) => Err(e),
    }
}

/// Store the evidence (platform transaction). Returns its id (`ev-NNNN`).
pub async fn insert(
    c: &mut PgConnection,
    run: &RunRow,
    actor: &Actor,
    by: &str,
    file: &str,
    format: Format,
    got: &Collected,
) -> ApiResult<String> {
    drive::assert_open(run)?;
    let distinct = i32::try_from(got.distinct.len()).unwrap_or(i32::MAX);
    let id: String = sqlx::query_scalar(
        "INSERT INTO generator_evidence (run_id, file, format, text_column, count_column, rows, distinct_rows, uploaded_by) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8) RETURNING id",
    )
    .bind(run.id)
    .bind(file)
    .bind(format.as_str())
    .bind(&got.text_column)
    .bind(got.count_column.as_deref())
    .bind(got.rows.max(distinct))
    .bind(distinct)
    .bind(by)
    .fetch_one(&mut *c)
    .await?;
    let hashes: Vec<&str> = got.distinct.iter().map(|d| d.text_hash.as_str()).collect();
    let excerpts: Vec<&str> = got.distinct.iter().map(|d| d.excerpt.as_str()).collect();
    let weights: Vec<i32> = got.distinct.iter().map(|d| d.weight).collect();
    let ords: Vec<i32> = (0..distinct).collect();
    sqlx::query(
        "INSERT INTO generator_evidence_row (evidence_id, text_hash, ord, excerpt, weight) \
         SELECT $1, u.h, u.o, nullif(u.e, ''), u.w \
           FROM UNNEST($2::text[], $3::integer[], $4::text[], $5::integer[]) AS u(h, o, e, w)",
    )
    .bind(&id)
    .bind(&hashes)
    .bind(&ords)
    .bind(&excerpts)
    .bind(&weights)
    .execute(&mut *c)
    .await?;
    store::touch(&mut *c, run.id).await?;
    drive::event(
        &mut *c,
        run,
        actor,
        "generator.evidence_upload",
        json!({"evidence": id, "format": format.as_str(), "rows": got.rows, "distinct_rows": distinct,
               "count_column": got.count_column.is_some()}),
    )
    .await?;
    Ok(id)
}

/// How a classification task ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Classified,
    /// No `gates` provider: the evidence stays `classifying`.
    Parked,
    /// The evidence (or its run) is gone.
    Cancelled,
    Failed,
}

/// Spawn [`classify`] for `evidence` of `run`.
pub fn spawn(st: AppState, run: i64, evidence: String) -> tokio::task::JoinHandle<Outcome> {
    tokio::spawn(async move { classify(&st, run, &evidence, None).await })
}

/// Boot: resume every evidence still `classifying` (never creates work).
pub async fn boot(st: &AppState) {
    let open: ApiResult<Vec<(i64, String)>> = async {
        let mut tx = begin_platform(&st.db).await?;
        let v = sqlx::query_as(
            "SELECT e.run_id, e.id FROM generator_evidence e JOIN generator_run r ON r.id = e.run_id \
              WHERE e.status = 'classifying' AND r.status = 'running' ORDER BY e.seq",
        )
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(v)
    }
    .await;
    match open {
        Ok(v) => {
            tracing::info!(evidence = v.len(), "generator_evidence_resume");
            for (run, id) in v {
                drop(spawn(st.clone(), run, id));
            }
        }
        Err(e) => tracing::warn!(error = %e, "generator_evidence_resume_failed"),
    }
}

/// The one Laya question (same shape as discovery's): which listed activity does the message ask for?
fn question(activities: &[Activity]) -> Option<EffGateQuestion> {
    let option = |key: &str, label: &str, description: String| {
        json!({"key": key, "label": label, "description": description, "generated": false,
               "base_label": label, "hidden": false, "refined": false, "verdicts": 0})
    };
    let mut options: Vec<Value> = activities
        .iter()
        .map(|a| option(&a.key, &a.label, format!("{} (stage {})", a.label, a.stage)))
        .collect();
    options.push(option(
        NONE_KEY,
        "None of these",
        "The message asks for something none of the listed activities does.".into(),
    ));
    serde_json::from_value(json!({
        "key": QUESTION_KEY, "group": "base", "layer": "base", "point": "P1", "type": "choice",
        "question": "Which one of the listed activities does this customer message ask for? Answer none when no \
                     activity fits.",
        "options": options, "generated_from": null, "threshold": "", "threshold_value": null,
        "fail": "open", "safety": false, "effect": "",
        "rule": {"fire_when": "in", "threshold": null, "fire_values": [NONE_KEY], "min_confidence": null},
        "action": "annotate", "tools": null, "origin": "platform", "updated_by": null,
        "updated_at": "2026-10-25T00:00:00Z", "base_threshold_value": null, "threshold_refined": false,
        "base_rule": {"fire_when": "in", "threshold": null, "fire_values": [NONE_KEY], "min_confidence": null},
        "rule_refined": false,
        "stats": {"verdicts_7d": 0, "fail_open": 0, "fail_closed": 0, "below_threshold": 0,
                  "latency_p50_ms": null, "latency_p95_ms": null, "timeouts": 0},
    }))
    .ok()
}

/// The activities the evidence is checked against: the run's live journey items, else the department's grid.
async fn activities(c: &mut PgConnection, run: &RunRow) -> ApiResult<Vec<Activity>> {
    let mut out: Vec<Activity> = Vec::new();
    for it in store::items_full(&mut *c, run.id).await? {
        if it.kind != "activity" || !matches!(it.decision.as_str(), "accepted" | "renamed") {
            continue;
        }
        let Some(key) = super::accept::activity_key(
            &it.decision,
            &it.title,
            it.payload.as_ref().and_then(|p| p["key"].as_str()),
        ) else {
            continue;
        };
        let Some((stage, _)) = key.split_once(':') else {
            continue;
        };
        let label = it
            .payload
            .as_ref()
            .and_then(|p| p["label"].as_str())
            .map_or_else(|| key.clone(), str::to_owned);
        out.push(Activity {
            key: key.clone(),
            stage: stage.to_owned(),
            label,
        });
    }
    if out.is_empty()
        && let Ok(j) = grid::load(&mut *c, &run.industry, &run.department).await
    {
        for a in &j.activities {
            out.push(Activity {
                key: a.key.clone(),
                stage: a.stage.as_str().to_owned(),
                label: a.label.clone(),
            });
        }
    }
    let mut seen = BTreeSet::new();
    out.retain(|a| seen.insert(a.key.clone()));
    Ok(out)
}

struct Asked {
    hash: String,
    bucket: Bucket,
    stage: Option<String>,
    activity: Option<String>,
    confidence: Option<f32>,
    used: Option<(Option<String>, LlmUsage)>,
}

async fn ask(
    laya: &LayaClient,
    q: &EffGateQuestion,
    acts: &[Activity],
    provider: &str,
    funding: &str,
    hash: String,
    text: Option<String>,
) -> Asked {
    let unclear = |hash: String| Asked {
        hash,
        bucket: Bucket::Unclear,
        stage: None,
        activity: None,
        confidence: None,
        used: None,
    };
    let Some(text) = text.filter(|t| !t.trim().is_empty()) else {
        return unclear(hash);
    };
    let started = Instant::now();
    let reply = match laya.ask(json!({"message": text}), &[q]).await {
        Ok(r) => r,
        Err(e) => {
            log_failure(&e);
            return unclear(hash);
        }
    };
    let usage = LlmUsage {
        provider_id: provider.to_owned(),
        model: reply
            .model
            .clone()
            .unwrap_or_else(|| laya.model().to_owned()),
        funding: funding.to_owned(),
        tokens_in: i64::try_from(reply.input_tokens).unwrap_or(i64::MAX),
        tokens_out: i64::try_from(reply.output_tokens).unwrap_or(i64::MAX),
        latency_ms: i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX),
    };
    let (bucket, act, confidence) = dclassify::verdict_of(reply.answers.get(QUESTION_KEY), acts);
    let key = act.as_ref().map(|a| a.key.clone());
    Asked {
        hash,
        bucket,
        stage: act.map(|a| a.stage),
        activity: key.clone(),
        confidence,
        used: Some((key, usage)),
    }
}

fn log_failure(e: &LayaError) {
    tracing::warn!(code = e.code(), "generator_evidence_laya_failed");
}

/// The Laya buckets stored for evidence: `hidden` does not apply to a platform grid.
fn bucket_name(b: Bucket) -> &'static str {
    match b {
        Bucket::Covered | Bucket::Hidden => "covered",
        Bucket::Uncovered => "uncovered",
        Bucket::Unclear => "unclear",
        Bucket::OutOfScope => "out_of_scope",
    }
}

async fn meter(
    st: &AppState,
    tenant: Uuid,
    actor: &str,
    use_: &'static str,
    used: &[(Option<String>, LlmUsage)],
) -> ApiResult<()> {
    if used.is_empty() {
        return Ok(());
    }
    let mut tx = tenancy::begin_scoped(&st.db, tenant).await?;
    for (activity, u) in used {
        usage::emit(
            &mut tx,
            &st.cfg.metering,
            &UsageEvent {
                job_id: None,
                source: super::usage::SOURCE,
                use_,
                activity: activity.as_deref(),
                conversation_id: None,
                case_id: None,
                actor,
                usage: u,
            },
        )
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Classify one evidence to its end. `drafter` overrides the platform drafter (tests).
pub async fn classify(
    st: &AppState,
    run_id: i64,
    evidence: &str,
    drafter: Option<Arc<dyn Drafter>>,
) -> Outcome {
    match classify_inner(st, run_id, evidence, drafter).await {
        Ok(o) => o,
        Err(e) => {
            tracing::warn!(evidence, error = ?e, "generator_evidence_failed");
            let failed: ApiResult<()> = async {
                let mut tx = begin_platform(&st.db).await?;
                sqlx::query("UPDATE generator_evidence SET status = 'failed', error = 'worker_error' WHERE id = $1 AND status = 'classifying'")
                    .bind(evidence)
                    .execute(&mut *tx)
                    .await?;
                tx.commit().await?;
                Ok(())
            }
            .await;
            if let Err(e) = failed {
                tracing::warn!(evidence, error = ?e, "generator_evidence_fail_mark_failed");
            }
            Outcome::Failed
        }
    }
}

async fn classify_inner(
    st: &AppState,
    run_id: i64,
    evidence: &str,
    drafter: Option<Arc<dyn Drafter>>,
) -> ApiResult<Outcome> {
    let Some(tenant) = st.cfg.platform_tenant_id else {
        return Ok(Outcome::Parked);
    };
    // what to classify
    let mut tx = begin_platform(&st.db).await?;
    let run = match store::run(&mut tx, run_id).await {
        Ok(r) if r.status == "running" => r,
        _ => return Ok(Outcome::Cancelled),
    };
    let row: Option<(String, String)> = sqlx::query_as(
        "SELECT status, uploaded_by FROM generator_evidence WHERE id = $1 AND run_id = $2",
    )
    .bind(evidence)
    .bind(run_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((status, actor)) = row else {
        return Ok(Outcome::Cancelled);
    };
    if status != "classifying" {
        return Ok(Outcome::Cancelled);
    }
    let acts = activities(&mut tx, &run).await?;
    let pending: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT text_hash, excerpt FROM generator_evidence_row WHERE evidence_id = $1 AND bucket IS NULL ORDER BY ord",
    )
    .bind(evidence)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;

    if !pending.is_empty() {
        let Ok(r) = gates_provider(st, &run).await else {
            return Ok(Outcome::Parked);
        };
        let laya = LayaClient::for_resolved(&r, &st.cfg.connector_policy, tenant)
            .await
            .ok();
        let q = question(&acts);
        let provider = r.provider_id.map_or_else(
            || crate::turn::qualify::INTERNAL_PROVIDER.to_owned(),
            |p| p.to_string(),
        );
        for batch in pending.chunks(BATCH) {
            let asked: Vec<Asked> = match (&laya, &q) {
                (Some(laya), Some(q)) if !acts.is_empty() => {
                    stream::iter(batch.to_vec())
                        .map(|(h, t)| ask(laya, q, &acts, &provider, r.funding, h, t))
                        .buffer_unordered(CONCURRENCY)
                        .collect()
                        .await
                }
                _ => batch
                    .iter()
                    .map(|(h, _)| Asked {
                        hash: h.clone(),
                        bucket: Bucket::Unclear,
                        stage: None,
                        activity: None,
                        confidence: None,
                        used: None,
                    })
                    .collect(),
            };
            let used: Vec<_> = asked.iter().filter_map(|a| a.used.clone()).collect();
            meter(st, tenant, &actor, USE_CLASSIFY, &used).await?;
            let mut tx = begin_platform(&st.db).await?;
            // the evidence (or its run) may have been removed meanwhile
            let still: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM generator_evidence WHERE id = $1)",
            )
            .bind(evidence)
            .fetch_one(&mut *tx)
            .await?;
            if !still {
                return Ok(Outcome::Cancelled);
            }
            for a in &asked {
                sqlx::query(
                    "UPDATE generator_evidence_row SET bucket = $3, stage = $4, activity = $5, confidence = $6 \
                      WHERE evidence_id = $1 AND text_hash = $2",
                )
                .bind(evidence)
                .bind(&a.hash)
                .bind(bucket_name(a.bucket))
                .bind(&a.stage)
                .bind(&a.activity)
                .bind(a.confidence)
                .execute(&mut *tx)
                .await?;
            }
            sqlx::query("UPDATE generator_evidence SET done = (SELECT count(*) FROM generator_evidence_row WHERE evidence_id = $1 AND bucket IS NOT NULL) WHERE id = $1")
                .bind(evidence)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
        }
    }

    // clusters of the uncovered rows (heaviest first)
    let mut tx = begin_platform(&st.db).await?;
    let unc: Vec<(String, Option<String>, i32)> = sqlx::query_as(
        "SELECT text_hash, excerpt, weight FROM generator_evidence_row \
          WHERE evidence_id = $1 AND bucket = 'uncovered' AND excerpt IS NOT NULL ORDER BY weight DESC, ord LIMIT $2",
    )
    .bind(evidence)
    .bind(i64::try_from(MAX_CLUSTER_ROWS).unwrap_or(200))
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    let rows: Vec<Uncovered> = unc
        .iter()
        .map(|(h, e, w)| Uncovered {
            text_hash: h.clone(),
            excerpt: e.clone().unwrap_or_default(),
            weight: *w,
        })
        .collect();
    let mut clusters: Vec<Value> = Vec::new();
    if !rows.is_empty() {
        let stages: BTreeSet<&str> = acts.iter().map(|a| a.stage.as_str()).collect();
        let d: Option<Arc<dyn Drafter>> = match drafter {
            Some(d) => Some(d),
            None => {
                let mut c = tenancy::begin_scoped(&st.db, tenant).await?;
                let d = OpenAiDrafter::platform(&mut c, st, &st.cfg.generator).await;
                c.commit().await?;
                d.ok().map(|d| Arc::new(d) as Arc<dyn Drafter>)
            }
        };
        if let Some(d) = d {
            let input = json!({
                "job": run.job_key,
                "stages": stages,
                "activities": acts.iter().map(|a| json!({"key": a.key, "label": a.label, "stage": a.stage})).collect::<Vec<_>>(),
                "messages": rows.iter().enumerate().map(|(i, r)| json!({"id": i, "text": r.excerpt})).collect::<Vec<_>>(),
            });
            match tokio::time::timeout(
                st.cfg.generator.draft_timeout,
                d.call(Call::Clusters, &input, 0),
            )
            .await
            {
                Ok(Ok((items, usage))) => {
                    meter(st, tenant, &actor, USE_CLUSTER, &[(None, usage.clone())]).await?;
                    let tokens = i32::try_from(usage.tokens_in.max(0) + usage.tokens_out.max(0))
                        .unwrap_or(i32::MAX);
                    let weight: BTreeMap<&str, i32> = rows
                        .iter()
                        .map(|r| (r.text_hash.as_str(), r.weight))
                        .collect();
                    for c in dclassify::parse_clusters(&items, &rows, &stages, tokens) {
                        let tickets: i64 = c
                            .members
                            .iter()
                            .map(|m| i64::from(weight.get(m.as_str()).copied().unwrap_or(1)))
                            .sum();
                        clusters.push(json!({"label": c.label, "tickets": tickets, "stage": c.suggestion["stage"],
                                             "suggestion": c.suggestion, "rows": c.members.len()}));
                    }
                }
                Ok(Err(e)) => tracing::warn!(error = ?e, "generator_evidence_cluster_failed"),
                Err(_) => tracing::warn!("generator_evidence_cluster_timeout"),
            }
        }
    }
    let mut tx = begin_platform(&st.db).await?;
    let n = sqlx::query("UPDATE generator_evidence SET status = 'classified', uncovered = $2 WHERE id = $1 AND status = 'classifying'")
        .bind(evidence)
        .bind(Value::Array(clusters))
        .execute(&mut *tx)
        .await?
        .rows_affected();
    tx.commit().await?;
    Ok(if n == 0 {
        Outcome::Cancelled
    } else {
        Outcome::Classified
    })
}
