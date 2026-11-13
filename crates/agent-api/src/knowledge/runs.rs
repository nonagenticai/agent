//! Re-sync runs of a job's knowledge sources (B3 T8; plan §5.4 `resyncSource` / `sourceRuns` / `sourceRun`, §4 0053
//! `source_run`). Ported from care-bus-agents@96c608c `crates/carebus-api/src/runs.rs`: a run is started through
//! strata_ingest, followed when it is READ (no background worker, D82), and on completion replaces the documents of
//! the source's earlier runs in strata_rag.
//!
//! - [`start`]: the request to strata_ingest is built (and an upload read) BEFORE the run row exists, so a refused
//!   request never leaves a run behind. A still-running previous run is refreshed first and then blocks (409
//!   `run_in_progress`; the `source_run_one_running` index closes the race). The source's credentials are opened only
//!   for that one request: they are never in a run row, an audit row or an error text.
//! - [`refresh`]: brings a running run up to date from strata_ingest's job views (a crawl / sitemap parent's page jobs
//!   included). strata_ingest unreachable: the run is answered as stored. A run with no job 60 s after it started was
//!   orphaned by a restart mid-start and fails.
//! - Every run request carries `source_run` = the run's id beside `source_id` (strata_ingest #14 stamps it on the
//!   documents): provenance only. It also carries the source's `supersedes` (PK-8, strata_ingest #15): top-level in a
//!   JSON body, a text field before the first file part of a file run; always sent, `[]` included, so a cleared list
//!   clears what strata stored on re-ingest.
//! - Replace (only a completed run with documents) never uses strata_rag's `except_run` prune (coordinator ruling: it
//!   deletes by `source_id`/`source_run` and cannot see shared documents). The run's recorded documents decide (CB):
//!   every document of an earlier run that this run did not produce again, that no other source's run still holds
//!   and that no direct upload into the same strata store produced (strata dedups on content) is deleted one by
//!   one; strata_ingest not answering for the uploads' jobs deletes nothing (all `failed`, retried). The report is
//!   `{deleted, already_gone, kept_shared, failed}`; an earlier run is marked `documents_removed` once none of its
//!   documents failed (a failed one is retried by the next completed run). A completed run with no
//!   document replaces nothing (the empty-sync guard), and a failed run never replaces.
//!
//! Every function opens its own short transaction per database step: none is held across a strata call.
use std::collections::{BTreeSet, HashMap, HashSet};

use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use futures_util::future::join_all;
use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::audit::{self, Actor, Event, Status};
use crate::error::{ApiError, ApiResult};
use crate::knowledge::sources::{self, MappingRow, SourceType, WebsiteMode};
use crate::netguard;
use crate::providers::secret::{self, SecretKind, SecretText};
use crate::state::AppState;
use crate::strata::rag::{self, DocumentsQuery};
use crate::strata::{self, Service, StrataStoreId, ingest};
use crate::tenancy::{self, JobCtx};

/// Most files one file-source re-sync takes (fixture `MAX_UPLOAD_FILES`).
pub const MAX_UPLOAD_FILES: usize = 20;
/// Most bytes one file-source re-sync takes in all (fixture `MAX_UPLOAD_BYTES`).
pub const MAX_UPLOAD_BYTES: usize = 50 * 1024 * 1024;
/// Runs per page of `sourceRuns` (fixture `paged(.., {limit: 20})`).
pub const RUNS_PAGE: i64 = 20;
/// The actor of the writes a read makes (finishing a run, replacing documents).
const SYSTEM: &str = "service:agent-api-source-runs";
/// Upstream calls made side by side while a run is followed or replaces.
const PARALLEL: usize = 8;
/// A running run with no recorded job this long after it started was orphaned by a restart mid-start.
const ORPHAN_AFTER_SECS: i64 = 60;
/// Longest run error message kept.
const MESSAGE_CHARS: usize = 500;
/// Documents a run detail lists (strata's page maximum).
const DETAIL_DOCUMENTS: i64 = 500;

/// The file types strata_ingest reads (`pipeline::detect` at `ce9590e`), by extension. Any other file of a re-sync
/// is reported on its upload (`unsupported_type`) and never sent: strata refuses the whole request for one bad part.
const INGESTIBLE: [&str; 28] = [
    "pdf", "docx", "pptx", "xlsx", "epub", "odt", "html", "htm", "txt", "md", "markdown", "csv",
    "json", "xml", "png", "jpg", "jpeg", "tif", "tiff", "webp", "bmp", "mp3", "wav", "flac", "ogg",
    "m4a", "mp4", "webm",
];

/// The job statuses after which strata_ingest changes a job no more (`deleting` included: it only disappears).
const ENDED: [&str; 5] = ["completed", "failed", "rejected", "cancelled", "deleting"];

/// A run's `mapping`: `same` / `edited` (the stored mapping or one sent with the re-sync) for api / datatable, else none.
fn mapping_word(kind: SourceType, edited: bool) -> Option<&'static str> {
    if !kind.structured() {
        None
    } else if edited {
        Some("edited")
    } else {
        Some("same")
    }
}

/// `{code, message}`.
fn run_error(code: &str, message: impl Into<String>) -> Value {
    let message: String = message.into();
    json!({"code": code, "message": message.chars().take(MESSAGE_CHARS).collect::<String>()})
}

// ---------------------------------------------------------------------------------------------------------------
// Rows

/// One `source_run` row as the API reads it.
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Run {
    pub id: Uuid,
    pub source_id: Uuid,
    pub status: String,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub started_by: String,
    pub ingest_job_ids: Vec<String>,
    pub document_ids: Vec<String>,
    pub documents: i32,
    pub pages: Option<Value>,
    pub error: Option<Value>,
    pub mapping: Option<String>,
    pub uploads: Option<Value>,
    pub replace_report: Option<Value>,
}

impl Run {
    /// UI `SourceRun` (`web/src/lib/api/types/tenant.ts`).
    pub fn json(&self) -> Value {
        json!({
            "id": self.id,
            "source_id": self.source_id,
            "status": self.status,
            "started_at": self.started_at,
            "finished_at": self.finished_at,
            "started_by": self.started_by,
            "documents": self.documents,
            "pages": self.pages,
            "error": self.error,
            "ingest_job_ids": self.ingest_job_ids,
            "mapping": self.mapping,
            "uploads": self.uploads,
            "replace_report": self.replace_report,
        })
    }

    fn running(&self) -> bool {
        self.status == "running"
    }
}

const RUN_SELECT: &str = "SELECT id, source_id, status, started_at, finished_at, started_by, ingest_job_ids, \
        document_ids, documents, pages, error, mapping, uploads, replace_report FROM source_run";

/// 404 `run_not_found`.
pub fn run_not_found() -> ApiError {
    ApiError::Missing(
        "run_not_found",
        "No run with that id for this source.".into(),
    )
}

/// Run `id` of source `source` in the job, or 404 `run_not_found`.
pub async fn load(st: &AppState, ctx: &JobCtx, source: Uuid, id: Uuid) -> ApiResult<Run> {
    let mut tx = ctx.begin(&st.db).await?;
    let run: Option<Run> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{RUN_SELECT} WHERE job_id = $1 AND source_id = $2 AND id = $3"
    )))
    .bind(ctx.job_id)
    .bind(source)
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    run.ok_or_else(run_not_found)
}

/// The running run of source `source`, if any.
async fn running_of(st: &AppState, ctx: &JobCtx, source: Uuid) -> ApiResult<Option<Uuid>> {
    let mut tx = ctx.begin(&st.db).await?;
    let id: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM source_run WHERE job_id = $1 AND source_id = $2 AND status = 'running'",
    )
    .bind(ctx.job_id)
    .bind(source)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(id)
}

/// Refreshes the running run of source `source`, if any; true while one is still running after that.
pub async fn still_running(st: &AppState, ctx: &JobCtx, source: Uuid) -> ApiResult<bool> {
    match running_of(st, ctx, source).await? {
        Some(r) => Ok(refresh(st, ctx, source, r).await?.running()),
        None => Ok(false),
    }
}

/// `sourceRuns`: newest first, [`RUNS_PAGE`] per page; `before` = the previous page's last `started_at`. The running
/// run is refreshed first. `next` is set when the page is full.
pub async fn page(
    st: &AppState,
    ctx: &JobCtx,
    source: Uuid,
    before: Option<DateTime<Utc>>,
) -> ApiResult<Value> {
    still_running(st, ctx, source).await?;
    let mut tx = ctx.begin(&st.db).await?;
    let rows: Vec<Run> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{RUN_SELECT} WHERE job_id = $1 AND source_id = $2 AND ($3::timestamptz IS NULL OR started_at < $3) \
          ORDER BY started_at DESC, id DESC LIMIT $4"
    )))
    .bind(ctx.job_id)
    .bind(source)
    .bind(before)
    .bind(RUNS_PAGE)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    let next = if i64::try_from(rows.len()).unwrap_or(0) == RUNS_PAGE {
        rows.last().map(|r| json!(r.started_at))
    } else {
        None
    };
    let items: Vec<Value> = rows.iter().map(Run::json).collect();
    Ok(json!({"items": items, "next": next}))
}

/// What a run needs of its source: the stored settings, the sealed key still sealed, and the target store's strata
/// binding.
#[derive(sqlx::FromRow)]
pub struct RunSource {
    pub id: Uuid,
    #[sqlx(rename = "type")]
    kind: String,
    config: Value,
    mapping: Value,
    pub target_store_id: Uuid,
    enabled: bool,
    language: String,
    credentials_sealed: Option<String>,
    strata_store_id: Option<Uuid>,
    /// PK-8: the platform topic keys the source's documents replace, sent with every run (strata_ingest #15).
    supersedes: Vec<String>,
}

impl RunSource {
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn kind(&self) -> ApiResult<SourceType> {
        SourceType::parse(&self.kind)
            .ok_or_else(|| ApiError::Internal(anyhow::anyhow!("unknown source type {}", self.kind)))
    }

    /// The strata store the source's documents land in (T4a: the pinned generation's store replaces this binding).
    pub fn strata(&self) -> Option<StrataStoreId> {
        self.strata_store_id.map(StrataStoreId)
    }

    /// The stored key, opened for one request (api / datatable only).
    fn secret(&self, st: &AppState, tenant: Uuid) -> ApiResult<Option<SecretText>> {
        match &self.credentials_sealed {
            Some(s) if self.kind()?.structured() => secret::open(
                st.provider_keyring.as_deref(),
                tenant,
                SecretKind::SourceCred,
                self.id,
                s,
            )
            .map(Some),
            _ => Ok(None),
        }
    }
}

/// Source `id` of the job (404 `source_not_found`).
pub async fn source(st: &AppState, ctx: &JobCtx, id: Uuid) -> ApiResult<RunSource> {
    let mut tx = ctx.begin(&st.db).await?;
    let s: Option<RunSource> = sqlx::query_as(
        "SELECT s.id, s.type, s.config, s.mapping, s.target_store_id, s.enabled, s.language, \
                s.credentials_sealed, k.strata_store_id, s.supersedes \
           FROM source s JOIN knowledge_store k ON k.id = s.target_store_id \
          WHERE s.job_id = $1 AND s.id = $2",
    )
    .bind(ctx.job_id)
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    s.ok_or_else(sources::source_not_found)
}

// ---------------------------------------------------------------------------------------------------------------
// Start

/// One uploaded file of a file-source re-sync. `bytes` is empty for a file refused before ingest.
pub struct Upload {
    pub file_name: String,
    pub content_type: Option<String>,
    pub size: usize,
    pub bytes: Vec<u8>,
}

/// A file strata_ingest reads, by its extension.
pub fn ingestible(file_name: &str) -> bool {
    file_name
        .rsplit_once('.')
        .is_some_and(|(_, e)| INGESTIBLE.contains(&e.to_ascii_lowercase().as_str()))
}

/// What a re-sync carries.
#[derive(Default)]
pub struct StartReq {
    /// An edited mapping (api / datatable): validated, stored on the source, used by this run.
    pub mapping: Option<Vec<MappingRow>>,
    /// The files of a file-source re-sync.
    pub files: Vec<Upload>,
    /// The audit `trigger`: `manual` (the re-sync button), `retry` (a failed feed job re-submitted, T6) or `schedule`
    /// (T8b, switched off).
    pub trigger: &'static str,
    /// The audit actor when it is not the request's caller (the scheduler's `System`, T8b).
    pub actor: Option<Actor>,
}

/// What [`start`] answers: 202 while the run follows its jobs, 200 once it has ended.
pub struct Started {
    pub status: StatusCode,
    pub run: Run,
}

/// 409 `source_disabled`.
pub fn source_disabled() -> ApiError {
    ApiError::Conflict(
        "source_disabled",
        "This source is disabled. Enable it before a re-sync.".into(),
    )
}

/// The ingest `options`: the source's `config.options`, with its language (D68) unless the options name one.
fn options_of(config: &Value, language: &str) -> Value {
    let mut o = config
        .get("options")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    if o.get("language").is_none_or(Value::is_null) {
        o.insert("language".into(), json!(language));
    }
    Value::Object(o)
}

/// A website source's strata_ingest body: `urls` mode is the plain `{urls}` request; `crawl` and `sitemap` are
/// `source: {kind, config}` with the start `url` and that mode's settings flattened into `config`.
fn website_body(config: &Value, store: StrataStoreId, source_id: &str, options: Value) -> Value {
    let mode = WebsiteMode::of(config).unwrap_or(WebsiteMode::Urls);
    let mut body = match mode {
        WebsiteMode::Urls => {
            let mut urls: Vec<Value> = config["urls"].as_array().cloned().unwrap_or_default();
            if let Some(u) = config.get("url").filter(|u| u.is_string()) {
                urls.push(u.clone());
            }
            json!({"store_id": store, "urls": urls, "source_id": source_id})
        }
        WebsiteMode::Crawl | WebsiteMode::Sitemap => {
            let kind = mode.as_str();
            let mut c = config
                .get(kind)
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            c.insert("url".into(), config["url"].clone());
            json!({"store_id": store, "source_id": source_id, "source": {"kind": kind, "config": c}})
        }
    };
    body["options"] = options;
    body
}

/// An api / datatable source's structured body (strata_ingest `api/sources.rs`): the key inline, for this request
/// only.
fn structured_body(
    kind: SourceType,
    config: &Value,
    mapping: &[MappingRow],
    store: StrataStoreId,
    source_id: &str,
    options: Value,
    key: Option<&SecretText>,
) -> Value {
    let mut config = config.clone();
    if let Some(o) = config.as_object_mut() {
        o.remove("options");
    }
    json!({
        "store_id": store,
        "source_id": source_id,
        "source": {
            "kind": kind.as_str(),
            "config": config,
            "credentials": key.map(|k| json!({"secret": k.expose()})),
        },
        "mapping": mapping,
        "options": options,
    })
}

/// `body` with the run's id as `source_run` (strata_ingest #14 forwards it to strata_rag: provenance only; agent
/// never prunes by it, replace deletes document by document) and the source's `supersedes` at the top level
/// (strata_ingest #15, applied to every record / page of the run; `[]` clears what strata stored).
fn with_run(mut body: Value, run: &str, supersedes: &[String]) -> Value {
    body["source_run"] = json!(run);
    sources::forward_supersedes(&mut body, supersedes);
    body
}

/// The request a run sends.
enum Request {
    Json(Value),
    Multipart(reqwest::multipart::Form),
    /// A file run whose every file was refused before ingest: nothing is sent.
    Nothing,
}

/// The multipart body of a file run (fields before the first file part, as strata_ingest requires; `supersedes`
/// among them, #15) and the `uploads[]` records, a refused file carrying its error.
fn file_request(
    files: Vec<Upload>,
    store: StrataStoreId,
    source_id: &str,
    run: &str,
    options: &Value,
    supersedes: &[String],
) -> ApiResult<(Request, Vec<Value>)> {
    let form = reqwest::multipart::Form::new()
        .text("store_id", store.to_string())
        .text("source_id", source_id.to_string())
        .text("source_run", run.to_string())
        .text("options", options.to_string());
    let mut form = sources::supersedes_field(form, supersedes);
    let mut records = Vec::with_capacity(files.len());
    let mut sent = 0usize;
    for f in files {
        let mut rec = json!({
            "file_name": f.file_name, "bytes": f.size, "content_type": f.content_type,
            "ingest_job_id": null, "error": null,
        });
        if !ingestible(&f.file_name) {
            rec["error"] = run_error(
                "unsupported_type",
                format!(
                    "{} is not a type strata_ingest reads (PDF, DOCX, PPTX, XLSX, HTML, TXT, MD, images, audio, \
                     video).",
                    f.file_name
                ),
            );
            records.push(rec);
            continue;
        }
        let mut part = reqwest::multipart::Part::bytes(f.bytes).file_name(f.file_name);
        if let Some(ct) = &f.content_type {
            part = part.mime_str(ct).map_err(|_| {
                ApiError::BadRequest(
                    "invalid_body",
                    "An uploaded file has an invalid content type.".into(),
                )
            })?;
        }
        form = form.part("file", part);
        records.push(rec);
        sent += 1;
    }
    Ok((
        if sent == 0 {
            Request::Nothing
        } else {
            Request::Multipart(form)
        },
        records,
    ))
}

/// Starts a run of source `id` (module doc). Refusals: 404 `source_not_found`; 409 `source_disabled`,
/// `run_in_progress`, `provision_pending`, `store_not_bound`; 422 `no_file`, `too_many_files`, `duplicate_field`,
/// `unknown_attribute`; 413 `too_large`. A strata_ingest refusal of the request is a run that ended `failed` (or
/// `unsupported` for a source kind it does not ingest); strata_ingest not answering fails the run and the request
/// (503).
pub async fn start(st: &AppState, ctx: &JobCtx, id: Uuid, req: StartReq) -> ApiResult<Started> {
    let tenant = ctx.t.tenant;
    let actor = match &req.actor {
        Some(a) => a.clone(),
        None => ctx.t.actor(),
    };
    let src = source(st, ctx, id).await?;
    let kind = src.kind()?;
    if !src.enabled {
        return Err(source_disabled());
    }
    if kind == SourceType::File {
        if req.files.is_empty() {
            return Err(ApiError::Unprocessable(
                "no_file",
                "Attach the files to ingest with the re-sync.".into(),
            ));
        }
        if req.files.len() > MAX_UPLOAD_FILES {
            return Err(ApiError::Unprocessable(
                "too_many_files",
                format!("A re-sync takes at most {MAX_UPLOAD_FILES} files."),
            ));
        }
        if req.files.iter().map(|f| f.size).sum::<usize>() > MAX_UPLOAD_BYTES {
            return Err(ApiError::TooLarge(
                "too_large",
                "A re-sync takes up to 50 MB of files.".into(),
            ));
        }
    }
    let edited = req.mapping.is_some();
    let mapping: Vec<MappingRow> = match &req.mapping {
        Some(rows) if kind.structured() => {
            let mut tx = ctx.begin(&st.db).await?;
            let targets =
                sources::mapping_targets(&mut tx, &st.design, st.compiler.as_ref(), ctx).await?;
            tx.commit().await?;
            sources::validate_mapping(&targets, rows)?
        }
        Some(_) => {
            return Err(sources::invalid(
                "Only api and datatable sources have a mapping.",
            ));
        }
        None => serde_json::from_value(src.mapping.clone()).unwrap_or_default(),
    };
    let p = {
        let mut tx = ctx.begin(&st.db).await?;
        let p = strata::gate(st, &mut tx, tenant, Service::Ingest).await?;
        tx.commit().await?;
        p
    };
    let Some(store) = src.strata() else {
        return Err(ApiError::Conflict(
            "store_not_bound",
            "The source's knowledge store is not created in strata_rag yet; create missing stores first.".into(),
        ));
    };
    if still_running(st, ctx, id).await? {
        return Err(sources::run_in_progress());
    }
    // the request, built before the run row exists (an upload is already read); it carries the run's id
    let run_id = Uuid::new_v4();
    let run_tag = run_id.to_string();
    let sid = sources::strata_source_id(id);
    let options = options_of(&src.config, &src.language);
    let key = if kind.structured() {
        src.secret(st, tenant)?
    } else {
        None
    };
    let (request, uploads) = match kind {
        SourceType::Website => (
            Request::Json(with_run(
                website_body(&src.config, store, &sid, options),
                &run_tag,
                &src.supersedes,
            )),
            None,
        ),
        SourceType::Api | SourceType::Datatable => (
            Request::Json(with_run(
                structured_body(
                    kind,
                    &src.config,
                    &mapping,
                    store,
                    &sid,
                    options,
                    key.as_ref(),
                ),
                &run_tag,
                &src.supersedes,
            )),
            None,
        ),
        SourceType::File => {
            let (r, recs) =
                file_request(req.files, store, &sid, &run_tag, &options, &src.supersedes)?;
            (r, Some(recs))
        }
    };

    {
        let mut tx = ctx.begin(&st.db).await?;
        tenancy::lock_tenant_row(&mut tx, tenant).await?;
        if edited {
            sqlx::query(
                "UPDATE source SET mapping = $3, updated_at = clock_timestamp(), updated_by = $4 \
                  WHERE job_id = $1 AND id = $2",
            )
            .bind(ctx.job_id)
            .bind(id)
            .bind(sqlx::types::Json(&mapping))
            .bind(&ctx.t.caller.actor)
            .execute(&mut *tx)
            .await?;
        }
        let inserted = sqlx::query(
            "INSERT INTO source_run (job_id, id, source_id, status, started_by, mapping, uploads) \
             VALUES ($1, $2, $3, 'running', $4, $5, $6)",
        )
        .bind(ctx.job_id)
        .bind(run_id)
        .bind(id)
        .bind(&ctx.t.caller.actor)
        .bind(mapping_word(kind, edited))
        .bind(uploads.clone().map(Value::Array))
        .execute(&mut *tx)
        .await;
        if let Err(e) = inserted {
            let busy = e
                .as_database_error()
                .and_then(|d| d.code())
                .is_some_and(|c| c == "23505");
            return Err(if busy {
                sources::run_in_progress()
            } else {
                ApiError::from(e)
            });
        }
        tx.commit().await?;
    }

    let sent = match request {
        Request::Json(body) => Some(ingest::create_json(&st.http, &p, body).await),
        Request::Multipart(form) => Some(ingest::create_multipart(&st.http, &p, form).await),
        Request::Nothing => None,
    };
    drop(key);
    let answered = match sent {
        None => {
            let err = run_error(
                "nothing_ingested",
                "No file could be ingested; the earlier documents are kept.",
            );
            finish(st, ctx, run_id, "failed", &[], 0, None, Some(err)).await?;
            Ok(StatusCode::OK)
        }
        Some(Ok(created)) => {
            let job_ids: Vec<String> = created.jobs.iter().map(|j| j.job_id.to_string()).collect();
            let uploads = uploads.map(|mut recs| {
                let mut jobs = created.jobs.iter();
                for r in recs.iter_mut().filter(|r| r["error"].is_null()) {
                    if let Some(j) = jobs.next() {
                        r["ingest_job_id"] = json!(j.job_id);
                        if j.status != "queued" {
                            r["error"] = run_error(
                                &j.status,
                                format!("strata_ingest {} this file.", j.status),
                            );
                        }
                    } else {
                        // strata answered fewer jobs than files: never a silent `ingest_job_id: null`
                        r["error"] = run_error(
                            "ingest_unconfirmed",
                            "strata_ingest answered no ingest job for this file.",
                        );
                    }
                }
                Value::Array(recs)
            });
            let mut tx = ctx.begin(&st.db).await?;
            sqlx::query(
                "UPDATE source_run SET ingest_job_ids = $2, uploads = COALESCE($3, uploads) WHERE id = $1",
            )
            .bind(run_id)
            .bind(&job_ids)
            .bind(uploads)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            // a structured source that could not be fetched or parsed is a 202 with its job already `failed`
            if created.jobs.iter().any(|j| j.status != "queued") {
                let run = refresh(st, ctx, id, run_id).await?;
                if !run.running() {
                    Ok(StatusCode::OK)
                } else {
                    Ok(StatusCode::ACCEPTED)
                }
            } else {
                Ok(StatusCode::ACCEPTED)
            }
        }
        Some(Err(e)) => {
            let (status, code) = e.status_and_code();
            if status.is_client_error() {
                // strata_ingest refused the request: the run ends here; only a kind it does not ingest is
                // `unsupported`
                let word = if code == "unsupported_source_kind" {
                    "unsupported"
                } else {
                    "failed"
                };
                let err = run_error(code, e.to_string());
                finish(st, ctx, run_id, word, &[], 0, None, Some(err)).await?;
                Ok(StatusCode::OK)
            } else {
                // a timeout or 5xx does not prove strata_ingest refused the jobs; it may have accepted them
                let err = run_error(
                    "ingest_unconfirmed",
                    "strata_ingest did not confirm the request; check Ingest jobs before re-syncing.",
                );
                finish(st, ctx, run_id, "failed", &[], 0, None, Some(err)).await?;
                Err(e)
            }
        }
    };
    let run = load(st, ctx, id, run_id).await?;
    let mut ev = Event::new("source.resync", "source", id.to_string())
        .job(&ctx.job_key)
        .detail("run_id", run_id.to_string())
        .detail("status", run.status.clone())
        .detail("trigger", req.trigger)
        .detail("jobs", run.ingest_job_ids.len());
    if let Some(m) = &run.mapping {
        ev = ev.detail("mapping", m.clone());
    }
    if let Some(u) = run.uploads.as_ref().and_then(Value::as_array) {
        ev = ev.detail("files", u.len());
    }
    if let Some(code) = run.error.as_ref().and_then(|e| e["code"].as_str()) {
        ev = ev.status(Status::Failure).detail("code", code.to_string());
    }
    audit::record_alone(&st.db, tenant, &actor, ev).await?;
    let status = answered?;
    Ok(Started { status, run })
}

// ---------------------------------------------------------------------------------------------------------------
// Refresh on read

/// `running` → final, only if still running. True for the one caller that won.
#[allow(clippy::too_many_arguments)]
async fn finish(
    st: &AppState,
    ctx: &JobCtx,
    id: Uuid,
    status: &str,
    docs: &[String],
    documents: usize,
    pages: Option<Value>,
    error: Option<Value>,
) -> ApiResult<bool> {
    let mut tx = ctx.begin(&st.db).await?;
    let done = sqlx::query(
        "UPDATE source_run SET status = $2, finished_at = clock_timestamp(), document_ids = $3, documents = $4, \
                pages = COALESCE($5, pages), error = $6 \
          WHERE id = $1 AND status = 'running'",
    )
    .bind(id)
    .bind(status)
    .bind(docs)
    .bind(i32::try_from(documents).unwrap_or(i32::MAX))
    .bind(pages)
    .bind(error)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(done.rows_affected() == 1)
}

/// A job's string field.
fn s<'v>(j: &'v Value, k: &str) -> &'v str {
    j.get(k).and_then(Value::as_str).unwrap_or_default()
}

fn ended(j: &Value) -> bool {
    ENDED.contains(&s(j, "status"))
}

/// The page jobs a crawl / sitemap parent lists.
fn child_ids(j: &Value) -> Vec<String> {
    j.get("child_job_ids")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|c| c.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// A parent whose page jobs are not all ended (`children.by_status`).
fn children_pending(j: &Value) -> bool {
    j.get("children")
        .and_then(|c| c.get("by_status"))
        .and_then(Value::as_object)
        .is_some_and(|m| {
            m.iter()
                .any(|(k, v)| !ENDED.contains(&k.as_str()) && v.as_i64().unwrap_or(0) > 0)
        })
}

/// A crawl or sitemap job: strata_ingest marks it with `children` / `child_job_ids`, and its source kind says so even
/// when it started no page at all.
fn is_parent(j: &Value) -> bool {
    j.get("children").is_some_and(Value::is_object)
        || !child_ids(j).is_empty()
        || matches!(
            j.get("source").map(|x| s(x, "kind")),
            Some("crawl" | "sitemap")
        )
}

/// `{total, by_status}` summed over the parent jobs; `None` when none is one.
fn pages_summary(jobs: &[Value]) -> Option<Value> {
    let parents: Vec<&Value> = jobs
        .iter()
        .filter_map(|j| j.get("children").filter(|c| c.is_object()))
        .collect();
    if parents.is_empty() {
        return None;
    }
    let mut total = 0i64;
    let mut by_status: Map<String, Value> = Map::new();
    for c in parents {
        total += c["total"].as_i64().unwrap_or(0);
        if let Some(m) = c["by_status"].as_object() {
            for (k, v) in m {
                let n =
                    by_status.get(k).and_then(Value::as_i64).unwrap_or(0) + v.as_i64().unwrap_or(0);
                by_status.insert(k.clone(), json!(n));
            }
        }
    }
    Some(json!({"total": total, "by_status": by_status}))
}

/// How a settled run ends: its documents (deduplicated, in order), `completed` or `failed`, and its error
/// (`{code, message}`, not yet scrubbed of the source's key).
#[derive(Debug, PartialEq)]
struct Outcome {
    docs: Vec<String>,
    status: &'static str,
    error: Option<(String, String)>,
}

/// The outcome of a run whose `jobs` (parents and their page jobs) have all ended; `missing` are job ids strata_ingest
/// no longer knows. A crawl or sitemap that discovered no page, or whose pages produced no document, FAILS:
/// completing it would let the replace delete every document of the source's earlier runs.
fn outcome(jobs: &[Value], missing: &[String]) -> Outcome {
    let mut seen = BTreeSet::new();
    let docs: Vec<String> = jobs
        .iter()
        .filter(|j| s(j, "status") == "completed")
        .flat_map(|j| {
            j.get("document_ids")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|d| d.as_str().map(str::to_string))
        })
        .filter(|d| seen.insert(d.clone()))
        .collect();
    let bad: Vec<&Value> = jobs
        .iter()
        .filter(|j| s(j, "status") != "completed")
        .collect();
    if !bad.is_empty() || !missing.is_empty() {
        let mut problems: Vec<String> = bad
            .iter()
            .map(|j| {
                let e = j.get("error").filter(|e| e.is_object());
                match e {
                    Some(e) => format!(
                        "{} {}: {}: {}",
                        s(j, "id"),
                        s(j, "status"),
                        s(e, "code"),
                        s(e, "message")
                    ),
                    None => format!("{} {}", s(j, "id"), s(j, "status")),
                }
            })
            .collect();
        problems.extend(
            missing
                .iter()
                .map(|m| format!("{m} is unknown to strata_ingest")),
        );
        // one failed job with a code of its own (a feed's `source_auth`, `no_records`, …) names the run's error
        let code = match (bad.as_slice(), missing.is_empty()) {
            ([one], true) => one
                .get("error")
                .map(|e| s(e, "code"))
                .filter(|c| !c.is_empty())
                .unwrap_or("ingest_failed")
                .to_string(),
            _ => "ingest_failed".to_string(),
        };
        let text = format!(
            "{} of {} ingest jobs did not complete: {}",
            problems.len(),
            jobs.len() + missing.len(),
            problems.join("; ")
        );
        return Outcome {
            docs,
            status: "failed",
            error: Some((code, text)),
        };
    }
    let parents: Vec<&Value> = jobs.iter().filter(|j| is_parent(j)).collect();
    let found: i64 = parents.iter().copied().map(page_count).sum();
    let error = if parents.iter().copied().any(|p| page_count(p) == 0) {
        Some((
            "no_pages".to_string(),
            "No pages discovered: the crawl or sitemap found no page to ingest; the source's earlier documents are \
             kept."
                .to_string(),
        ))
    } else if !parents.is_empty() && docs.is_empty() {
        Some((
            "no_documents".to_string(),
            format!(
                "The {found} pages discovered produced no document; the source's earlier documents are kept."
            ),
        ))
    } else {
        None
    };
    Outcome {
        docs,
        status: if error.is_some() {
            "failed"
        } else {
            "completed"
        },
        error,
    }
}

/// The pages a crawl / sitemap parent discovered (`children.total`, at least its listed page jobs).
fn page_count(p: &Value) -> i64 {
    p.get("children")
        .and_then(|c| c["total"].as_i64())
        .unwrap_or(0)
        .max(i64::try_from(child_ids(p).len()).unwrap_or(i64::MAX))
}

/// The views of `ids` (in order), fetched [`PARALLEL`] at a time: `Ok(None)` for a job strata_ingest does not know,
/// `Err` when it did not answer.
async fn views(
    st: &AppState,
    p: &strata::Plane<'_>,
    ids: &[String],
) -> ApiResult<Vec<(String, Option<Value>)>> {
    let mut out = Vec::with_capacity(ids.len());
    for chunk in ids.chunks(PARALLEL) {
        let got = join_all(chunk.iter().map(|id| async move {
            match Uuid::parse_str(id) {
                Ok(u) => match ingest::get(&st.http, p, u).await {
                    Ok(v) => Ok(Some(v)),
                    Err(e) if e.code() == "not_found" => Ok(None),
                    Err(e) => Err(e),
                },
                Err(_) => Ok(None),
            }
        }))
        .await;
        for (id, r) in chunk.iter().zip(got) {
            out.push((id.clone(), r?));
        }
    }
    Ok(out)
}

/// The source's key, opened only to scrub texts (none, no keyring or unreadable: `None`).
async fn scrub_key(st: &AppState, ctx: &JobCtx, source_id: Uuid) -> Option<SecretText> {
    let src = source(st, ctx, source_id).await.ok()?;
    src.secret(st, ctx.t.tenant).ok().flatten()
}

/// `text` without the source's key or any `KEY:value` secret.
fn scrubbed(text: &str, key: Option<&str>) -> String {
    let t = match key {
        Some(k) => netguard::redact_text(text, k),
        None => text.to_string(),
    };
    match netguard::scrub_secrets(Value::String(t)) {
        Value::String(t) => t,
        _ => String::new(),
    }
}

/// Brings run `id` of source `source` up to date from strata_ingest (module doc). Any run that is not running is
/// returned as stored.
pub async fn refresh(st: &AppState, ctx: &JobCtx, source: Uuid, id: Uuid) -> ApiResult<Run> {
    let run = load(st, ctx, source, id).await?;
    if !run.running() {
        return Ok(run);
    }
    if run.ingest_job_ids.is_empty() {
        if (Utc::now() - run.started_at).num_seconds() > ORPHAN_AFTER_SECS {
            let err = run_error(
                "run_orphaned",
                "No ingest job was recorded for this run; the service stopped while starting it. Re-sync again.",
            );
            finish(st, ctx, id, "failed", &[], 0, None, Some(err)).await?;
            return load(st, ctx, source, id).await;
        }
        return Ok(run);
    }
    // strata is set up for the tenant (the run's jobs exist there): config only, not the provisioning gate
    let Ok(p) = strata::plane(st, ctx.t.tenant, Service::Ingest).await else {
        return Ok(run);
    };
    let Ok(parents) = views(st, &p, &run.ingest_job_ids).await else {
        // strata_ingest did not answer: the run as stored
        return Ok(run);
    };
    let mut jobs: Vec<Value> = Vec::new();
    let mut missing: Vec<String> = Vec::new();
    for (jid, v) in parents {
        match v {
            Some(v) => jobs.push(v),
            None => missing.push(jid),
        }
    }
    let pages = pages_summary(&jobs);
    if jobs.iter().any(|j| !ended(j) || children_pending(j)) {
        if pages.is_some() && pages != run.pages {
            let mut tx = ctx.begin(&st.db).await?;
            sqlx::query("UPDATE source_run SET pages = $2 WHERE id = $1 AND status = 'running'")
                .bind(id)
                .bind(&pages)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            return load(st, ctx, source, id).await;
        }
        return Ok(run);
    }
    // a crawl's or sitemap's documents come from its page jobs: they count as the run's jobs
    let children: Vec<String> = jobs.iter().flat_map(child_ids).collect();
    let Ok(kids) = views(st, &p, &children).await else {
        return Ok(run);
    };
    for (cid, v) in kids {
        match v {
            Some(v) => jobs.push(v),
            None => missing.push(cid),
        }
    }
    if jobs.iter().any(|j| !ended(j)) {
        return Ok(run);
    }
    let Outcome {
        docs,
        mut status,
        mut error,
    } = outcome(&jobs, &missing);
    // a file run that left files out (refused before ingest) fails: the accepted files' documents are added and
    // nothing earlier is deleted
    let refused = run
        .uploads
        .as_ref()
        .and_then(Value::as_array)
        .map_or(0, |u| u.iter().filter(|x| !x["error"].is_null()).count());
    if refused > 0 && error.is_none() {
        status = "failed";
        let total = run
            .uploads
            .as_ref()
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        error = Some((
            "partial_upload".to_string(),
            format!(
                "{refused} of {total} files were not ingested; the accepted files were added and the earlier \
                 documents are kept."
            ),
        ));
    }
    let error = match error {
        Some((code, text)) => {
            let key = scrub_key(st, ctx, source).await;
            Some(run_error(
                &code,
                scrubbed(&text, key.as_ref().map(SecretText::expose)),
            ))
        }
        None => None,
    };
    let documents = docs.len();
    if finish(st, ctx, id, status, &docs, documents, pages, error).await? && status == "completed" {
        replace(st, ctx, source, id).await?;
    }
    load(st, ctx, source, id).await
}

// ---------------------------------------------------------------------------------------------------------------
// Replace

/// `{deleted, already_gone, kept_shared, failed}`.
fn report(deleted: u64, already_gone: u64, kept_shared: usize, failed: &[String]) -> Value {
    json!({"deleted": deleted, "already_gone": already_gone, "kept_shared": kept_shared, "failed": failed})
}

/// An earlier run of the source whose documents are still held.
#[derive(sqlx::FromRow)]
struct Prior {
    id: Uuid,
    document_ids: Vec<String>,
    status: String,
}

/// The subset of `docs` that a run of ANOTHER source still holds. strata_rag dedups on `(store, content_hash)`, so
/// two sources feeding one leaf with the same content share one document; deleting it for one source would empty the
/// other. Read across every job of the tenant (strata stores are tenant-level).
async fn held_elsewhere(
    st: &AppState,
    ctx: &JobCtx,
    source: Uuid,
    docs: &[String],
) -> ApiResult<HashSet<String>> {
    if docs.is_empty() {
        return Ok(HashSet::new());
    }
    let mut tx = ctx.t.begin(&st.db).await?;
    let rows: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT d FROM source_run, unnest(document_ids) AS d \
          WHERE source_id <> $1 AND NOT documents_removed AND d = ANY($2)",
    )
    .bind(source)
    .bind(docs)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(rows.into_iter().collect())
}

/// The documents that direct uploads (`upload:<id>`, T6) into the source's strata store produced, from their ingest
/// jobs' views. strata_rag dedups an upload onto a source's document with the same content (the first writer's
/// `source_id` stays), so deleting that document for the source would empty the upload. `Err` when strata_ingest did
/// not answer: then nothing may be deleted. Uploads into another strata store cannot share a document.
async fn held_by_uploads(
    st: &AppState,
    ctx: &JobCtx,
    source_id: Uuid,
) -> ApiResult<HashSet<String>> {
    let Some(store) = source(st, ctx, source_id).await?.strata() else {
        return Ok(HashSet::new());
    };
    let mut tx = ctx.t.begin(&st.db).await?;
    let jobs: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT j FROM knowledge_upload u \
           JOIN knowledge_store k ON k.tenant_id = u.tenant_id AND k.id = u.store_id \
           CROSS JOIN LATERAL unnest(u.ingest_job_ids) AS j \
          WHERE k.strata_store_id = $1",
    )
    .bind(store.0)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    if jobs.is_empty() {
        return Ok(HashSet::new());
    }
    let p = strata::plane(st, ctx.t.tenant, Service::Ingest).await?;
    let mut held = HashSet::new();
    for (_, v) in views(st, &p, &jobs).await? {
        let Some(v) = v else { continue };
        let ids = v.get("document_ids").and_then(Value::as_array);
        held.extend(
            ids.into_iter()
                .flatten()
                .filter_map(|d| d.as_str().map(str::to_string)),
        );
    }
    Ok(held)
}

/// Completed run `id` replaces the documents of the source's earlier runs (module doc).
async fn replace(st: &AppState, ctx: &JobCtx, source_id: Uuid, id: Uuid) -> ApiResult<()> {
    let run = load(st, ctx, source_id, id).await?;
    let mut tx = ctx.begin(&st.db).await?;
    let prior: Vec<Prior> = sqlx::query_as(
        "SELECT id, document_ids, status FROM source_run \
          WHERE source_id = $1 AND id <> $2 AND status <> 'running' AND NOT documents_removed \
            AND (started_at, id) < ($3, $2) \
          ORDER BY started_at, id",
    )
    .bind(source_id)
    .bind(id)
    .bind(run.started_at)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    let keep: HashSet<&str> = run.document_ids.iter().map(String::as_str).collect();
    let candidates: Vec<String> = prior
        .iter()
        .flat_map(|p| p.document_ids.iter().cloned())
        .collect();
    let elsewhere = held_elsewhere(st, ctx, source_id, &candidates).await?;
    // documents an upload shares are kept too; strata_ingest not answering for them: nothing is deleted this time
    let uploaded = if candidates.is_empty() || run.document_ids.is_empty() {
        Ok(HashSet::new())
    } else {
        held_by_uploads(st, ctx, source_id).await
    };
    let mut seen: HashSet<&str> = HashSet::new();
    let mut targets: Vec<String> = Vec::new();
    let mut shared = 0usize;
    for d in &candidates {
        if !seen.insert(d.as_str()) {
            continue;
        }
        let by_upload = uploaded.as_ref().is_ok_and(|u| u.contains(d));
        if keep.contains(d.as_str()) || elsewhere.contains(d) || by_upload {
            shared += 1;
        } else {
            targets.push(d.clone());
        }
    }
    let (deleted, gone, failed) = if run.document_ids.is_empty() {
        // the empty-sync guard: a run that produced nothing replaces nothing
        (0, 0, Vec::new())
    } else if uploaded.is_err() {
        // unknown whether an upload shares them: every target is retried by the next completed run
        let mut failed = targets;
        failed.sort();
        (0, 0, failed)
    } else {
        delete_earlier(st, ctx, &targets).await
    };
    let failed_set: HashSet<&str> = failed.iter().map(String::as_str).collect();
    let rep = report(deleted, gone, shared, &failed);
    let replaced = prior
        .iter()
        .rev()
        .find(|p| p.status == "completed")
        .map(|p| p.id);
    let mut tx = ctx.begin(&st.db).await?;
    tenancy::lock_tenant_row(&mut tx, ctx.t.tenant).await?;
    if !run.document_ids.is_empty() {
        let removed: Vec<Uuid> = prior
            .iter()
            .filter(|p| {
                p.document_ids
                    .iter()
                    .all(|d| !failed_set.contains(d.as_str()))
            })
            .map(|p| p.id)
            .collect();
        sqlx::query("UPDATE source_run SET documents_removed = true WHERE id = ANY($1)")
            .bind(&removed)
            .execute(&mut *tx)
            .await?;
    }
    sqlx::query("UPDATE source_run SET replaced_run_id = $2, replace_report = $3 WHERE id = $1")
        .bind(id)
        .bind(replaced)
        .bind(&rep)
        .execute(&mut *tx)
        .await?;
    let mut ev = Event::new("source.replace", "source", source_id.to_string())
        .job(&ctx.job_key)
        .detail("run_id", id.to_string())
        .detail("deleted", deleted)
        .detail("already_gone", gone)
        .detail("kept_shared", shared)
        .detail("failed", failed.len());
    if !failed.is_empty() {
        ev = ev.status(Status::Failure);
    }
    audit::record(&mut tx, &Actor::system(SYSTEM), ev).await?;
    tx.commit().await?;
    Ok(())
}

/// Deletes `targets` (earlier documents of the source this run no longer has, none shared with another source or an
/// upload) one by one: `(deleted, already_gone, failed)`. Never strata_rag's `except_run` prune: it deletes by
/// `source_id` / `source_run` and cannot see a document another source or an upload shares (coordinator ruling).
async fn delete_earlier(
    st: &AppState,
    ctx: &JobCtx,
    targets: &[String],
) -> (u64, u64, Vec<String>) {
    let Ok(p) = strata::plane(st, ctx.t.tenant, Service::Rag).await else {
        return (0, 0, targets.to_vec());
    };
    let (mut deleted, mut gone) = (0u64, 0u64);
    let mut failed: Vec<String> = Vec::new();
    for chunk in targets.chunks(PARALLEL) {
        let got = join_all(chunk.iter().map(|d| {
            let p = &p;
            async move {
                match Uuid::parse_str(d) {
                    Ok(u) => rag::delete_document(&st.http, p, u).await.map(|()| true),
                    // never a strata document id: nothing to delete
                    Err(_) => Ok(false),
                }
            }
        }))
        .await;
        for (d, r) in chunk.iter().zip(got) {
            match r {
                Ok(true) => deleted += 1,
                Ok(false) => gone += 1,
                Err(e) if e.code() == "not_found" => gone += 1,
                Err(_) => failed.push(d.clone()),
            }
        }
    }
    failed.sort();
    (deleted, gone, failed)
}

// ---------------------------------------------------------------------------------------------------------------
// Run detail

/// `sourceRun`: the run (refreshed), its ingest jobs as UI `IngestJob`s and the documents it produced as UI
/// `KnowledgeDocument`s (`language` = the source's, D68). strata_ingest not answering: the jobs are left out.
pub async fn detail(st: &AppState, ctx: &JobCtx, source_id: Uuid, id: Uuid) -> ApiResult<Value> {
    let src = source(st, ctx, source_id).await?;
    let run = refresh(st, ctx, source_id, id).await?;
    let tenant = ctx.t.tenant;
    let mut jobs: Vec<Value> = Vec::new();
    if !run.ingest_job_ids.is_empty()
        && let Ok(p) = strata::plane(st, tenant, Service::Ingest).await
        && let Ok(found) = views(st, &p, &run.ingest_job_ids).await
    {
        jobs = found.into_iter().filter_map(|(_, v)| v).collect();
    }
    let mut job_of: HashMap<String, String> = HashMap::new();
    for j in &jobs {
        for d in j
            .get("document_ids")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(d) = d.as_str() {
                job_of.insert(d.to_string(), s(j, "id").to_string());
            }
        }
    }
    // strata_ingest never echoes a key; the source's own key is still taken out of every text shown
    let key = src.secret(st, tenant).ok().flatten();
    let ingest_jobs: Vec<Value> = jobs
        .iter()
        .map(|j| {
            let v = ingest::ui_view(j, Some(src.target_store_id), &run.started_by);
            match &key {
                Some(k) => netguard::redact(v, k.expose()),
                None => v,
            }
        })
        .collect();
    drop(key);
    let mut documents: Vec<Value> = Vec::new();
    if !run.document_ids.is_empty()
        && let Some(store) = src.strata()
    {
        let p = strata::plane(st, tenant, Service::Rag).await?;
        let q = DocumentsQuery {
            limit: Some(DETAIL_DOCUMENTS),
            cursor: None,
            source_id: Some(sources::strata_source_id(source_id)),
        };
        let page = rag::documents(&st.http, &p, store, &q).await?;
        let mine: HashSet<&str> = run.document_ids.iter().map(String::as_str).collect();
        let mut docs: Vec<&rag::Document> = page
            .items
            .iter()
            .filter(|d| mine.contains(d.id.to_string().as_str()))
            .collect();
        docs.sort_by_key(|d| d.title.clone().unwrap_or_else(|| d.source.clone()));
        documents = docs
            .into_iter()
            .map(|d| {
                json!({
                    "id": d.id,
                    "store_id": src.target_store_id,
                    "title": d.title.clone().unwrap_or_else(|| d.source.clone()),
                    "source": d.source,
                    "tokens": d.token_count,
                    "chunks": d.chunk_count,
                    "added_at": d.created_at,
                    "ingest_job_id": job_of.get(&d.id.to_string()),
                    "language": src.language,
                })
            })
            .collect();
    }
    Ok(json!({"run": run.json(), "ingest_jobs": ingest_jobs, "documents": documents}))
}

// ---------------------------------------------------------------------------------------------------------------
// Retry of a failed feed job (T6 `retryIngestJob`)

/// The source a failed strata_ingest job of a structured feed belongs to: `(job id, job key, source id)` for an api /
/// datatable job; `None` for any other job. strata_ingest keeps no credentials, so such a job is re-run from its
/// source: one whose source is gone (no uuid, or no row) is 409 `source_gone`.
pub async fn source_of_job(
    st: &AppState,
    t: &tenancy::TenantCtx,
    job: &Value,
) -> ApiResult<Option<(Uuid, String, Uuid)>> {
    let kind = job.get("source").map(|x| s(x, "kind")).unwrap_or_default();
    if !matches!(kind, "api" | "datatable") {
        return Ok(None);
    }
    let gone = || {
        ApiError::Conflict(
            "source_gone",
            "strata_ingest keeps no credentials, so this job can only be re-run from its source, which no longer \
             exists. Create the source again and re-sync it."
                .into(),
        )
    };
    let sid = ingest::source_id(job)
        .and_then(|x| Uuid::parse_str(x).ok())
        .ok_or_else(gone)?;
    let mut tx = t.begin(&st.db).await?;
    let row: Option<(Uuid, String)> = sqlx::query_as(
        "SELECT s.job_id, j.key FROM source s JOIN jobs j ON j.id = s.job_id WHERE s.id = $1",
    )
    .bind(sid)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    let (job_id, key) = row.ok_or_else(gone)?;
    Ok(Some((job_id, key, sid)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> StrataStoreId {
        StrataStoreId(Uuid::parse_str("7f1c9a2e-0000-4000-8000-0000000000a1").unwrap())
    }

    #[test]
    fn website_bodies_per_mode() {
        let st = store();
        let cfg = json!({"urls": ["https://help.example.nl/a"], "options": {"captions": false}});
        assert_eq!(
            website_body(&cfg, st, "src", options_of(&cfg, "nl")),
            json!({"store_id": st, "source_id": "src", "urls": ["https://help.example.nl/a"],
                   "options": {"captions": false, "language": "nl"}})
        );
        let crawl = json!({"mode": "crawl", "url": "https://help.example.nl/",
                           "crawl": {"max_depth": 3}, "options": {"language": "en"}});
        assert_eq!(
            website_body(&crawl, st, "src", options_of(&crawl, "nl")),
            json!({"store_id": st, "source_id": "src", "options": {"language": "en"},
                   "source": {"kind": "crawl", "config": {"url": "https://help.example.nl/", "max_depth": 3}}})
        );
    }

    #[test]
    fn structured_body_carries_the_key_inline_and_no_options_in_config() {
        let cfg = json!({"url": "https://api.example.com/p", "auth": {"type": "bearer"}, "options": {"ocr": "auto"}});
        let b = structured_body(
            SourceType::Api,
            &cfg,
            &[],
            store(),
            "src",
            options_of(&cfg, "en"),
            None,
        );
        assert_eq!(b["source"]["credentials"], Value::Null);
        assert!(b["source"]["config"].get("options").is_none());
        assert_eq!(b["options"], json!({"ocr": "auto", "language": "en"}));
    }

    #[test]
    fn a_run_body_carries_its_run_and_the_source_supersedes_top_level() {
        let b = with_run(json!({"store_id": "s"}), "run-8", &["hr/leave".to_string()]);
        assert_eq!(
            b,
            json!({"store_id": "s", "source_run": "run-8", "supersedes": ["hr/leave"]})
        );
        assert_eq!(with_run(json!({}), "r", &[])["supersedes"], json!([]));
    }

    fn job(id: &str, status: &str, docs: &[&str]) -> Value {
        json!({"id": id, "status": status, "document_ids": docs, "source": {"kind": "url"}})
    }

    #[test]
    fn crawl_with_no_page_or_no_document_fails() {
        let parent = json!({"id": "p", "status": "completed", "document_ids": [], "source": {"kind": "sitemap"},
                            "children": {"total": 0, "by_status": {}}});
        let o = outcome(&[parent], &[]);
        assert_eq!(o.status, "failed");
        assert_eq!(o.error.unwrap().0, "no_pages");
        let parent = json!({"id": "p", "status": "completed", "document_ids": [], "source": {"kind": "crawl"},
                            "children": {"total": 1, "by_status": {"completed": 1}}, "child_job_ids": ["c"]});
        let o = outcome(&[parent, job("c", "completed", &[])], &[]);
        assert_eq!(o.error.unwrap().0, "no_documents");
    }

    #[test]
    fn one_failed_job_names_the_run_error_and_docs_dedupe() {
        let mut bad = job("b", "failed", &[]);
        bad["error"] = json!({"code": "source_auth", "message": "HTTP 401"});
        let o = outcome(&[job("a", "completed", &["d1", "d2"]), bad], &[]);
        assert_eq!(o.status, "failed");
        let (code, text) = o.error.unwrap();
        assert_eq!(code, "source_auth");
        assert!(
            text.starts_with("1 of 2 ingest jobs did not complete"),
            "{text}"
        );
        let o = outcome(
            &[
                job("a", "completed", &["d1", "d2"]),
                job("b", "completed", &["d2", "d3"]),
            ],
            &[],
        );
        assert_eq!(
            o,
            Outcome {
                docs: vec!["d1".into(), "d2".into(), "d3".into()],
                status: "completed",
                error: None
            }
        );
    }

    #[test]
    fn pages_sum_and_pending_children() {
        let a = json!({"children": {"total": 3, "by_status": {"completed": 2, "running": 1}}});
        let b = json!({"children": {"total": 2, "by_status": {"completed": 1, "failed": 1}}});
        assert!(children_pending(&a) && !children_pending(&b));
        assert_eq!(
            pages_summary(&[a, json!({}), b]),
            Some(json!({"total": 5, "by_status": {"completed": 3, "running": 1, "failed": 1}}))
        );
        assert_eq!(pages_summary(&[json!({})]), None);
    }

    #[test]
    fn scrubbed_removes_the_key() {
        let t = scrubbed("HTTP 401 with sk-live-123456 sent", Some("sk-live-123456"));
        assert!(!t.contains("sk-live-123456"), "{t}");
        assert!(ingestible("Returns.PDF") && !ingestible("setup.exe") && !ingestible("noext"));
    }
}
