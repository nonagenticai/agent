//! `/api/jobs/{job}/sources`: one job's knowledge sources (B3 T7; plan §5.4, §7 G3). Shapes = `web/src/lib/api/types/
//! tenant.ts` (`Source`, `SourceInput`, `SourceSample`, `ColumnPreview`, `MappingTarget`); refusals = the fixture
//! (`web/src/lib/api/areas/tenant.ts` T9). Ported from care-bus-agents@96c608c `routes/sources.rs`; crate::knowledge::
//! sources holds the validation and the rows.
//!
//! - `GET …/sources` (member) → `Paged<Source>` (by name, each with its last run). `GET …/sources/{id}` (member) →
//!   `Source`, 404 `source_not_found`. `GET …/sources/mapping-targets` (member) → `MappingTarget[]` (the job's
//!   draft ontology attributes).
//! - `POST …/sources` (knowledge_manager) → 201 `Source`; 422 `invalid_source`, `address_refused`,
//!   `invalid_target_store`, `language_not_served`, `invalid_schedule`, `duplicate_field`, `unknown_attribute`,
//!   `secret_rejected`; 409 `source_exists`; 503 `provider_secret_key_unconfigured` for a key without a keyring.
//! - `PUT …/sources/{id}` (knowledge_manager) → `Source`; as create + 422 `type_fixed`. `credentials` absent = keep,
//!   `null` = clear, `{secret}` = replace (audit `credentials: kept|cleared|replaced`). Keeping a stored key while
//!   the URL moves to another origin is 422 `secret_required_for_new_host` (a key only goes to the host it was saved
//!   for; as providers and `testSource`).
//! - Create and save take `supersedes` (PK-8, T6): the platform topic keys the source replaces, 422 `invalid_source`
//!   per crate::knowledge::sources::normalize_supersedes; absent on a save keeps the stored list. `Source` returns
//!   it (`[]` when none) and the audit detail names the list stored.
//! - `DELETE …/sources/{id}?purge=` (knowledge_manager) → `{purged, documents_deleted, documents_kept}`; 409
//!   `run_in_progress`. With `purge=true` strata_rag `DELETE /api/sources/<id>/documents?all=true` runs first
//!   (no transaction open across it); a failed purge keeps the source.
//! - `POST …/sources/test` and `POST …/sources/{id}/test` (knowledge_manager; stores nothing) → `SourceSample`
//!   `{ok, status, detail, fields, sample}`: ONE GET of the source URL through `netguard::pinned_client`, with the key
//!   per `config.auth` (typed, else the saved one, and the saved one only to the saved origin). 422 `not_testable`
//!   (file), `address_refused`, `invalid_source`, `secret_required_for_new_host`. A host that answers an error, or
//!   does not answer, is `ok: false`.
//! - `POST …/sources/preview-columns` (knowledge_manager; stores nothing) → `ColumnPreview`. JSON = the draft
//!   (`{type, config, credentials?, sheet?, header_row?}`; the table is fetched from `config.url` through netguard),
//!   or multipart `file` (+ `sheet`, `header_row`, `type`). 422 `not_a_table`, `invalid_source`, `address_refused`,
//!   `sheet_not_found`, `unreadable_table`; 413 `too_large` (10 MB); 415 `unsupported_type`.
//!
//! - `POST …/sources/{id}/resync` (knowledge_manager; T8) → 202 `SourceRun` while it follows its jobs, 200 once
//!   ended. JSON `{mapping?}` (an edited mapping: validated, stored, the run says `mapping: "edited"`) or, for a file
//!   source, multipart `file` parts (at most 20, 50 MB in all; a type strata_ingest does not read is reported on its
//!   upload and not sent). 409 `source_disabled`, `run_in_progress`, `provision_pending`, `store_not_bound`; 422
//!   `no_file`, `too_many_files`, mapping codes; 413 `too_large`. crate::knowledge::runs does the work.
//! - `GET …/sources/{id}/runs?before=` (member) → `Paged<SourceRun>`, newest first, 20 per page; the running run is
//!   refreshed from strata_ingest first. `GET …/sources/{id}/runs/{run}` (member) → `SourceRunDetail`
//!   `{run, ingest_jobs, documents}`; 404 `run_not_found` (another source's or job's run too).
//!
//! `schedule` is stored and shown, never run here (D82; the scheduler is T8b's and is switched off).
use std::time::Duration;

use axum::body::to_bytes;
use axum::extract::multipart::MultipartError;
use axum::extract::{DefaultBodyLimit, FromRequest, Multipart, Request, State};
use axum::http::StatusCode;
use axum::http::header::CONTENT_TYPE;
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use reqwest::Url;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::audit::scrub;
use crate::audit::{self, Event};
use crate::error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult};
use crate::knowledge::runs::{self, StartReq};
use crate::knowledge::sources::{
    self, CredentialsChange, MappingRow, Scope, SealedChange, SourceType, Stored, invalid,
};
use crate::netguard;
use crate::state::AppState;
use crate::strata::rag::{self, Prune, PruneScope};
use crate::strata::{self, Service};
use crate::tabular::{self, ColumnPreview, PreviewError, TableFormat};
use crate::tenancy::{self, JobCtx, JobMember, JobRequires, role};

/// Largest table a column preview reads (fixture `MAX_PREVIEW_BYTES`).
pub const MAX_PREVIEW_BYTES: usize = 10 * 1024 * 1024;
/// The preview route's body limit: the file plus its form fields.
const PREVIEW_BODY_LIMIT: usize = MAX_PREVIEW_BYTES + 64 * 1024;
/// Largest JSON body of a preview (a draft source).
const PREVIEW_JSON_LIMIT: usize = 64 * 1024;
/// Longest a column preview may parse before it answers 422 `unreadable_table`.
const PREVIEW_TIMEOUT: Duration = Duration::from_secs(10);
/// The re-sync route's body limit: a file source's files plus their form framing.
const RESYNC_BODY_LIMIT: usize = runs::MAX_UPLOAD_BYTES + 1024 * 1024;
/// Largest JSON body of a feed re-sync (`{mapping}`).
const RESYNC_JSON_LIMIT: usize = 64 * 1024;
/// One test connection or table fetch: DNS + connect + the whole answer.
const FETCH_TIMEOUT: Duration = Duration::from_secs(15);

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/jobs/{job}/sources", get(list).post(create))
        // static segments win over `{id}` in axum's router
        .route("/api/jobs/{job}/sources/mapping-targets", get(targets))
        .route("/api/jobs/{job}/sources/test", post(test_unsaved))
        .route(
            "/api/jobs/{job}/sources/preview-columns",
            post(preview_columns).layer(DefaultBodyLimit::max(PREVIEW_BODY_LIMIT)),
        )
        .route(
            "/api/jobs/{job}/sources/{id}",
            get(one).put(save).delete(remove),
        )
        .route("/api/jobs/{job}/sources/{id}/test", post(test_saved))
        .route(
            "/api/jobs/{job}/sources/{id}/resync",
            post(resync).layer(DefaultBodyLimit::max(RESYNC_BODY_LIMIT)),
        )
        .route("/api/jobs/{job}/sources/{id}/runs", get(runs_list))
        .route("/api/jobs/{job}/sources/{id}/runs/{run}", get(run_one))
}

/// `{id}` under `/api/jobs/{job}` (serde ignores the `job` key).
#[derive(Deserialize)]
struct IdPath {
    id: Uuid,
}

// ---------------------------------------------------------------------------------------------------------------
// Reads

/// `sources`.
async fn list(JobMember(ctx): JobMember, State(st): State<AppState>) -> ApiResult<Json<Value>> {
    let mut tx = ctx.begin(&st.db).await?;
    let items = sources::list(&mut tx, &ctx).await?;
    tx.commit().await?;
    Ok(Json(json!({"items": items, "next": null})))
}

/// `source`.
async fn one(
    JobMember(ctx): JobMember,
    State(st): State<AppState>,
    ApiPath(IdPath { id }): ApiPath<IdPath>,
) -> ApiResult<Json<Value>> {
    let mut tx = ctx.begin(&st.db).await?;
    let v = sources::view(&mut tx, &ctx, id).await?;
    tx.commit().await?;
    Ok(Json(v))
}

/// `sourceMappingTargets`: the attributes of the job's draft ontology.
async fn targets(JobMember(ctx): JobMember, State(st): State<AppState>) -> ApiResult<Json<Value>> {
    let mut tx = ctx.begin(&st.db).await?;
    let items = sources::mapping_targets(&mut tx, &st.design, st.compiler.as_ref(), &ctx).await?;
    tx.commit().await?;
    Ok(Json(json!(items)))
}

// ---------------------------------------------------------------------------------------------------------------
// Writes

fn scope<'a>(st: &'a AppState, ctx: &'a JobCtx, stored: Option<&'a Stored>) -> Scope<'a> {
    Scope {
        reg: &st.design,
        compiler: st.compiler.as_ref(),
        policy: st.connectors.policy(),
        ctx,
        self_id: stored.map(|s| s.id),
        stored_language: stored.map(|s| s.language.as_str()),
    }
}

/// `createSource`: validated, the key sealed for the new row, inserted and audited in one transaction.
async fn create(
    JobRequires(ctx, _): JobRequires<role::KnowledgeManager>,
    State(st): State<AppState>,
    ApiJson(raw): ApiJson<Value>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let p = sources::parse(raw)?;
    let mut tx = ctx.begin(&st.db).await?;
    tenancy::lock_tenant_row(&mut tx, ctx.t.tenant).await?;
    let ck = sources::validate(&mut tx, &scope(&st, &ctx, None), &p.input).await?;
    let id = Uuid::new_v4();
    let sealed = match &p.credentials {
        CredentialsChange::Set(c) => {
            sources::check_credentials_allowed(p.input.kind, &p.input.config, true)?;
            Some(sources::seal(
                st.provider_keyring.as_deref(),
                ctx.t.tenant,
                id,
                c,
            )?)
        }
        CredentialsChange::Keep | CredentialsChange::Clear => None,
    };
    sources::insert(&mut tx, &ctx, id, &p.input, &ck, sealed.as_ref()).await?;
    audit::record(
        &mut tx,
        &ctx.t.actor(),
        Event::new("source.create", "source", id.to_string())
            .job(&ctx.job_key)
            .detail("type", p.input.kind.as_str())
            .detail("language", ck.language.clone())
            .detail("credentials", if sealed.is_some() { "set" } else { "none" })
            .detail("supersedes", ck.supersedes.clone().unwrap_or_default()),
    )
    .await?;
    let v = sources::view(&mut tx, &ctx, id).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(v)))
}

/// `saveSource`: 404 before the body is checked; the type is fixed.
async fn save(
    JobRequires(ctx, _): JobRequires<role::KnowledgeManager>,
    State(st): State<AppState>,
    ApiPath(IdPath { id }): ApiPath<IdPath>,
    ApiJson(raw): ApiJson<Value>,
) -> ApiResult<Json<Value>> {
    let mut tx = ctx.begin(&st.db).await?;
    tenancy::lock_tenant_row(&mut tx, ctx.t.tenant).await?;
    let s = sources::stored(&mut tx, &ctx, id, true).await?;
    let p = sources::parse(raw)?;
    if p.input.kind != s.kind()? {
        return Err(ApiError::Unprocessable(
            "type_fixed",
            "The type of a saved source cannot change.".into(),
        ));
    }
    let ck = sources::validate(&mut tx, &scope(&st, &ctx, Some(&s)), &p.input).await?;
    let change = match &p.credentials {
        CredentialsChange::Keep
            if s.has_credentials() && !keeps_origin(&s.config, &p.input.config) =>
        {
            return Err(new_host());
        }
        CredentialsChange::Keep => SealedChange::Keep,
        CredentialsChange::Clear => SealedChange::Clear,
        CredentialsChange::Set(c) => {
            sources::check_credentials_allowed(p.input.kind, &p.input.config, true)?;
            SealedChange::Set(sources::seal(
                st.provider_keyring.as_deref(),
                ctx.t.tenant,
                id,
                c,
            )?)
        }
    };
    sources::update(&mut tx, &ctx, id, &p.input, &ck, &change).await?;
    let mut ev = Event::new("source.update", "source", id.to_string())
        .job(&ctx.job_key)
        .detail("type", p.input.kind.as_str())
        .detail("credentials", change.word());
    // the new list when the body names one; absent = the stored list was kept
    if let Some(keys) = &ck.supersedes {
        ev = ev.detail("supersedes", keys.clone());
    }
    audit::record(&mut tx, &ctx.t.actor(), ev).await?;
    let v = sources::view(&mut tx, &ctx, id).await?;
    tx.commit().await?;
    Ok(Json(v))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeleteQuery {
    #[serde(default)]
    purge: bool,
}

/// `deleteSource`: refused while a run is running; with `purge` the source's documents are deleted from strata_rag
/// first (outside any transaction), then the source and its runs here.
async fn remove(
    JobRequires(ctx, _): JobRequires<role::KnowledgeManager>,
    State(st): State<AppState>,
    ApiPath(IdPath { id }): ApiPath<IdPath>,
    ApiQuery(q): ApiQuery<DeleteQuery>,
) -> ApiResult<Json<Value>> {
    // a run whose jobs have all ended finishes here first (T8 refresh-on-read), so it does not block the delete
    if runs::still_running(&st, &ctx, id).await? {
        return Err(sources::run_in_progress());
    }
    let mut tx = ctx.begin(&st.db).await?;
    tenancy::lock_tenant_row(&mut tx, ctx.t.tenant).await?;
    let s = sources::stored(&mut tx, &ctx, id, true).await?;
    if sources::running(&mut tx, id).await? {
        return Err(sources::run_in_progress());
    }
    let held = sources::documents_held(&mut tx, id, s.kind()?).await?;
    tx.commit().await?;
    let (deleted, kept) = if q.purge {
        // documents exist in strata only if strata is set up for the tenant: config, not the provisioning gate
        let p = strata::plane(&st, ctx.t.tenant, Service::Rag)
            .await?
            .with_actor(&ctx.t.caller.sub);
        let prune = Prune {
            scope: PruneScope::All,
            store_id: None,
            allow_empty: true,
        };
        let r = rag::prune_source(&st.http, &p, &sources::strata_source_id(id), &prune).await?;
        (i64::try_from(r.deleted).unwrap_or(i64::MAX), r.kept.max(0))
    } else {
        (0, held)
    };
    let mut tx = ctx.begin(&st.db).await?;
    tenancy::lock_tenant_row(&mut tx, ctx.t.tenant).await?;
    sources::stored(&mut tx, &ctx, id, true).await?;
    // a re-sync started during the purge would be cascaded away while its ingest jobs keep writing
    if sources::running(&mut tx, id).await? {
        return Err(sources::run_in_progress());
    }
    sources::delete(&mut tx, &ctx, id).await?;
    audit::record(
        &mut tx,
        &ctx.t.actor(),
        Event::new("source.delete", "source", id.to_string())
            .job(&ctx.job_key)
            .detail("purge", q.purge)
            .detail("documents_deleted", deleted)
            .detail("documents_kept", kept),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(json!({
        "purged": q.purge,
        "documents_deleted": deleted,
        "documents_kept": kept,
    })))
}

// ---------------------------------------------------------------------------------------------------------------
// Re-sync runs (T8)

/// `{job}/{id}/{run}`.
#[derive(Deserialize)]
struct RunPath {
    id: Uuid,
    run: Uuid,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RunsQuery {
    /// The previous page's `next` (a run's `started_at`).
    #[serde(default)]
    before: Option<DateTime<Utc>>,
}

/// `{mapping?}` of a feed re-sync.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResyncBody {
    #[serde(default)]
    mapping: Option<Vec<MappingRow>>,
}

fn upload_too_large() -> ApiError {
    ApiError::TooLarge("too_large", "A re-sync takes up to 50 MB of files.".into())
}

/// A body over the route's limit is too large; any other parse failure is a malformed body.
fn resync_multipart_error(e: MultipartError) -> ApiError {
    if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
        upload_too_large()
    } else {
        sources::bad_body("The multipart body is not valid.")
    }
}

/// A file source's `file` parts, refused as soon as a limit is crossed (other fields are skipped). A file of a type
/// strata_ingest does not read is drained, not kept: the run reports it on its upload.
async fn read_uploads(req: Request) -> ApiResult<Vec<runs::Upload>> {
    let mut mp = Multipart::from_request(req, &()).await.map_err(|_| {
        sources::bad_body("A file source re-sync takes a multipart upload with file fields.")
    })?;
    let mut out: Vec<runs::Upload> = Vec::new();
    let mut total = 0usize;
    while let Some(mut field) = mp.next_field().await.map_err(resync_multipart_error)? {
        if field.name() != Some("file") {
            continue;
        }
        let file_name = field
            .file_name()
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map(str::to_string)
            .ok_or_else(|| sources::bad_body("Every file field needs a file name."))?;
        if out.len() == runs::MAX_UPLOAD_FILES {
            return Err(ApiError::Unprocessable(
                "too_many_files",
                format!("A re-sync takes at most {} files.", runs::MAX_UPLOAD_FILES),
            ));
        }
        let content_type = field.content_type().map(str::to_string);
        let keep = runs::ingestible(&file_name);
        let mut bytes: Vec<u8> = Vec::new();
        let mut size = 0usize;
        while let Some(chunk) = field.chunk().await.map_err(resync_multipart_error)? {
            size += chunk.len();
            total += chunk.len();
            if total > runs::MAX_UPLOAD_BYTES {
                return Err(upload_too_large());
            }
            if keep {
                bytes.extend_from_slice(&chunk);
            }
        }
        out.push(runs::Upload {
            file_name,
            content_type,
            size,
            bytes,
        });
    }
    Ok(out)
}

/// A feed re-sync's optional `{mapping}` (an empty body = the stored mapping). A secret anywhere in it is 422
/// `secret_rejected`.
async fn read_mapping(req: Request) -> ApiResult<Option<Vec<MappingRow>>> {
    let bytes = to_bytes(req.into_body(), RESYNC_JSON_LIMIT)
        .await
        .map_err(|_| sources::bad_body("The re-sync body is not JSON of at most 64 KB."))?;
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(None);
    }
    let raw: Value = serde_json::from_slice(&bytes)
        .map_err(|_| sources::bad_body("The re-sync body must be JSON."))?;
    if raw.is_null() {
        return Ok(None);
    }
    scrub::check(&raw).map_err(|f| {
        ApiError::Unprocessable(
            "secret_rejected",
            format!("{f} looks like a secret; a key goes in the source's credentials."),
        )
    })?;
    let b: ResyncBody = serde_json::from_value(raw)
        .map_err(|e| sources::bad_body(format!("The re-sync body is not valid: {e}")))?;
    Ok(b.mapping)
}

/// `resyncSource`.
async fn resync(
    JobRequires(ctx, _): JobRequires<role::KnowledgeManager>,
    State(st): State<AppState>,
    ApiPath(IdPath { id }): ApiPath<IdPath>,
    req: Request,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let src = runs::source(&st, &ctx, id).await?;
    if !src.enabled() {
        return Err(runs::source_disabled());
    }
    let mut sr = StartReq {
        trigger: "manual",
        ..StartReq::default()
    };
    if src.kind()? == SourceType::File {
        sr.files = read_uploads(req).await?;
    } else {
        sr.mapping = read_mapping(req).await?;
    }
    let started = runs::start(&st, &ctx, id, sr).await?;
    Ok((started.status, Json(started.run.json())))
}

/// `sourceRuns`.
async fn runs_list(
    JobMember(ctx): JobMember,
    State(st): State<AppState>,
    ApiPath(IdPath { id }): ApiPath<IdPath>,
    ApiQuery(q): ApiQuery<RunsQuery>,
) -> ApiResult<Json<Value>> {
    // 404 for an unknown source, never an empty page
    runs::source(&st, &ctx, id).await?;
    Ok(Json(runs::page(&st, &ctx, id, q.before).await?))
}

/// `sourceRun`.
async fn run_one(
    JobMember(ctx): JobMember,
    State(st): State<AppState>,
    ApiPath(RunPath { id, run }): ApiPath<RunPath>,
) -> ApiResult<Json<Value>> {
    Ok(Json(runs::detail(&st, &ctx, id, run).await?))
}

// ---------------------------------------------------------------------------------------------------------------
// Test connection (stores nothing)

/// `testSource(null, draft)`.
async fn test_unsaved(
    JobRequires(ctx, _): JobRequires<role::KnowledgeManager>,
    State(st): State<AppState>,
    ApiJson(raw): ApiJson<Value>,
) -> ApiResult<Json<Value>> {
    test_with(&st, &ctx, raw, None).await.map(Json)
}

/// `testSource(id, draft)`: the draft's settings, the saved key when the draft names none.
async fn test_saved(
    JobRequires(ctx, _): JobRequires<role::KnowledgeManager>,
    State(st): State<AppState>,
    ApiPath(IdPath { id }): ApiPath<IdPath>,
    ApiJson(raw): ApiJson<Value>,
) -> ApiResult<Json<Value>> {
    let mut tx = ctx.begin(&st.db).await?;
    let saved = sources::stored(&mut tx, &ctx, id, false).await?;
    tx.commit().await?;
    test_with(&st, &ctx, raw, Some(&saved)).await.map(Json)
}

/// The body as an object, `credentials` split off.
fn draft_of(raw: Value) -> ApiResult<(Map<String, Value>, CredentialsChange)> {
    let Value::Object(mut o) = raw else {
        return Err(sources::bad_body("The request body must be a JSON object."));
    };
    let change = sources::credentials_change(o.remove("credentials"))?;
    Ok((o, change))
}

/// The same scheme, host and port: a saved key may follow a path edit, never a different host.
fn same_origin(a: &str, b: &str) -> bool {
    match (Url::parse(a.trim()), Url::parse(b.trim())) {
        (Ok(a), Ok(b)) => a.origin() == b.origin(),
        _ => false,
    }
}

/// The URL of `new` has the origin of `old`'s: a stored key may stay (both without a URL counts as the same).
fn keeps_origin(old: &Value, new: &Value) -> bool {
    match (sources::source_url(old), sources::source_url(new)) {
        (Some(a), Some(b)) => same_origin(&a, &b),
        (None, None) => true,
        _ => false,
    }
}

/// 422 `secret_required_for_new_host`: a stored key is never sent to a host it was not saved for.
fn new_host() -> ApiError {
    ApiError::Unprocessable(
        "secret_required_for_new_host",
        crate::upstream::message_for("secret_required_for_new_host")
            .unwrap_or(
                "Enter the key again: a stored key is only sent to the host it was saved for.",
            )
            .into(),
    )
}

/// `host[:port]` of a checked URL, for details.
fn host_of(u: &Url) -> String {
    match (u.host_str(), u.port()) {
        (Some(h), Some(p)) => format!("{h}:{p}"),
        (Some(h), None) => h.to_string(),
        _ => String::new(),
    }
}

/// A `SourceSample`.
fn sample(ok: bool, status: Option<u16>, detail: String, fields: Vec<String>, s: Value) -> Value {
    json!({"ok": ok, "status": status, "detail": detail, "fields": fields, "sample": s})
}

/// What one guarded GET answered.
struct Fetched {
    status: StatusCode,
    content_type: String,
    body: Vec<u8>,
    /// The body was longer than the cap (only the first `cap` bytes are kept).
    truncated: bool,
}

/// Why a guarded GET did not answer.
enum FetchError {
    /// The host resolves to a non-public address: 422 `address_refused`.
    Refused(String),
    /// No answer (unresolved, unreachable, timed out): a probe `ok: false`.
    NoAnswer(String),
}

/// ONE GET of `url` through `netguard::pinned_client`, the key presented per `config.auth`, the body read up to
/// `cap` bytes.
async fn fetch(
    st: &AppState,
    tenant: Uuid,
    url: &Url,
    config: &Value,
    secret: Option<&str>,
    cap: usize,
) -> Result<Fetched, FetchError> {
    let policy = st.connectors.policy();
    let client = match netguard::pinned_client(url, policy, tenant, FETCH_TIMEOUT).await {
        Ok(c) => c,
        Err(e) if e.code == "invalid_target" => return Err(FetchError::Refused(e.detail)),
        Err(e) => return Err(FetchError::NoAnswer(e.detail)),
    };
    let mut req = client.get(url.clone());
    match (sources::auth_type(config), secret) {
        ("bearer", Some(s)) => req = req.bearer_auth(s),
        ("header", Some(s)) => {
            let name = config["auth"]["header_name"].as_str().unwrap_or_default();
            req = req.header(name, s);
        }
        _ => {}
    }
    let mut resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            let why = if e.is_timeout() {
                format!("no answer within {} s", FETCH_TIMEOUT.as_secs())
            } else {
                "the host could not be reached".to_string()
            };
            return Err(FetchError::NoAnswer(why));
        }
    };
    let status = resp.status();
    let content_type = resp
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let mut body: Vec<u8> = Vec::new();
    let mut truncated = false;
    loop {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                let room = cap.saturating_sub(body.len());
                if chunk.len() > room {
                    body.extend_from_slice(&chunk[..room]);
                    truncated = true;
                    break;
                }
                body.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(_) => {
                truncated = true;
                break;
            }
        }
    }
    Ok(Fetched {
        status,
        content_type,
        body,
        truncated,
    })
}

/// The test of a draft (and its saved source).
async fn test_with(
    st: &AppState,
    ctx: &JobCtx,
    raw: Value,
    saved: Option<&Stored>,
) -> ApiResult<Value> {
    let (mut o, change) = draft_of(raw)?;
    let kind = match o.get("type").and_then(Value::as_str) {
        Some(t) => SourceType::parse(t)
            .ok_or_else(|| invalid("type must be website, api, file or datatable."))?,
        None => match saved {
            Some(s) => s.kind()?,
            None => return Err(invalid("type is required.")),
        },
    };
    if kind == SourceType::File {
        return Err(ApiError::Unprocessable(
            "not_testable",
            "File sources have nothing to connect to.".into(),
        ));
    }
    let config = match o.remove("config") {
        Some(c) => c,
        None => saved.map(|s| s.config.clone()).unwrap_or_else(|| json!({})),
    };
    let raw_url = sources::source_url(&config)
        .ok_or_else(|| invalid("Enter the URL the source reads from."))?;
    let tenant = ctx.t.tenant;
    let url = sources::check_source_url(&raw_url, st.connectors.policy(), tenant)?;
    sources::validate_auth(config.get("auth"))?;
    let uses_key = kind.structured() && sources::auth_type(&config) != "none";
    let secret: Option<String> = match change {
        CredentialsChange::Set(c) => Some(c.secret),
        CredentialsChange::Clear => None,
        CredentialsChange::Keep => match saved {
            Some(s) if uses_key && s.has_credentials() => {
                let saved_url = sources::source_url(&s.config).unwrap_or_default();
                if !same_origin(&raw_url, &saved_url) {
                    return Err(new_host());
                }
                s.secret(st.provider_keyring.as_deref(), tenant)?
                    .map(|t| t.expose().to_string())
            }
            _ => None,
        },
    };
    let secret = secret.filter(|_| uses_key);
    let host = host_of(&url);
    let got = match fetch(
        st,
        tenant,
        &url,
        &config,
        secret.as_deref(),
        netguard::MAX_BODY_BYTES,
    )
    .await
    {
        Ok(f) => f,
        Err(FetchError::Refused(detail)) => {
            return Err(ApiError::Unprocessable("address_refused", detail));
        }
        Err(FetchError::NoAnswer(why)) => {
            return Ok(sample(
                false,
                None,
                format!("{host}: {why}"),
                Vec::new(),
                Value::Null,
            ));
        }
    };
    let code = got.status.as_u16();
    let ct = if got.content_type.is_empty() {
        "no content type".to_string()
    } else {
        got.content_type.clone()
    };
    let out = if !got.status.is_success() {
        let mut detail = format!("HTTP {code} from {host}");
        if code == 401 || code == 403 {
            detail.push_str(if secret.is_none() {
                ": no key was sent. Enter the key in Credentials."
            } else {
                ": the key was rejected."
            });
        }
        sample(false, Some(code), detail, Vec::new(), Value::Null)
    } else {
        match kind {
            SourceType::Website => sample(
                true,
                Some(code),
                format!("Start page fetched from {host} · {ct}"),
                Vec::new(),
                Value::Null,
            ),
            SourceType::Datatable => table_sample(&got, &config, &host, &ct).await,
            SourceType::Api | SourceType::File => {
                let records = serde_json::from_slice::<Value>(&got.body)
                    .ok()
                    .map(|v| v.as_array().map_or(1, Vec::len));
                let (s, fields) = netguard::sample_of(&got.body);
                let detail = match records {
                    Some(n) if !got.truncated => format!("{host} returned {ct} · {n} records"),
                    _ => format!("{host} returned {ct}"),
                };
                sample(true, Some(code), detail, fields, s)
            }
        }
    };
    Ok(match &secret {
        Some(s) => {
            let mut out = netguard::redact(out, s);
            if let Some(d) = out["detail"].as_str() {
                out["detail"] = json!(netguard::redact_text(d, s));
            }
            out
        }
        None => out,
    })
}

/// A datatable test: the columns and the first row of the (first MiB of the) table.
async fn table_sample(got: &Fetched, config: &Value, host: &str, ct: &str) -> Value {
    let code = got.status.as_u16();
    let format = table_format(config, None, ct);
    let header_row = config["header_row"].as_bool().unwrap_or(true);
    let sheet = config["sheet"].as_str().map(str::to_string);
    let parsed = match format {
        // a cut XLSX is not a zip any more
        Some(TableFormat::Xlsx) if got.truncated => None,
        Some(f) => {
            let bytes = got.body.clone();
            off_thread(move || parse_table(f, &bytes, sheet.as_deref(), header_row))
                .await
                .ok()
                .and_then(Result::ok)
        }
        None => None,
    };
    match parsed {
        Some(p) => {
            let first = p
                .sample_rows
                .first()
                .map(|r| row_object(&p.columns, r))
                .unwrap_or(Value::Null);
            let rows = if got.truncated {
                format!("at least {} rows", p.row_count)
            } else {
                format!("{} rows", p.row_count)
            };
            sample(
                true,
                Some(code),
                format!("{host} returned {ct} · {rows}"),
                p.columns,
                first,
            )
        }
        None => sample(
            true,
            Some(code),
            format!("{host} returned {ct}; open the column preview to read the table"),
            Vec::new(),
            Value::Null,
        ),
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Column preview (stores nothing)

/// A table to preview.
struct Table {
    file: String,
    format: TableFormat,
    bytes: Vec<u8>,
    sheet: Option<String>,
    header_row: bool,
}

fn not_a_table() -> ApiError {
    ApiError::Unprocessable("not_a_table", "Only datatable sources have columns.".into())
}

fn too_large() -> ApiError {
    ApiError::TooLarge("too_large", "Previews take files up to 10 MB.".into())
}

fn unsupported_type() -> ApiError {
    ApiError::Unsupported(
        "unsupported_type",
        "Only .csv and .xlsx files can be previewed.".into(),
    )
}

/// A body over the route's limit is too large; any other parse failure is a malformed body.
fn multipart_error(e: MultipartError) -> ApiError {
    if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
        too_large()
    } else {
        sources::bad_body("The multipart body is not valid.")
    }
}

/// The table format: `config.format`, else the file name's extension, else the content type.
fn table_format(config: &Value, file: Option<&str>, content_type: &str) -> Option<TableFormat> {
    match config["format"].as_str() {
        Some("csv") => return Some(TableFormat::Csv),
        Some("xlsx") => return Some(TableFormat::Xlsx),
        _ => {}
    }
    if let Some(f) = file.and_then(TableFormat::from_file_name) {
        return Some(f);
    }
    let ct = content_type.to_ascii_lowercase();
    if ct.contains("spreadsheetml") {
        Some(TableFormat::Xlsx)
    } else if ct.contains("csv") {
        Some(TableFormat::Csv)
    } else {
        None
    }
}

/// `header_row` form / JSON value.
fn header_row_of(v: &str) -> ApiResult<bool> {
    match v.trim() {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(sources::bad_body(
            "The header_row field must be true or false.",
        )),
    }
}

/// `previewSourceColumns`.
async fn preview_columns(
    JobRequires(ctx, _): JobRequires<role::KnowledgeManager>,
    State(st): State<AppState>,
    req: Request,
) -> ApiResult<Json<Value>> {
    let multipart = req
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.to_ascii_lowercase().starts_with("multipart/form-data"));
    let t = if multipart {
        uploaded_table(req).await?
    } else {
        fetched_table(&st, &ctx, req).await?
    };
    let Table {
        file,
        format,
        bytes,
        sheet,
        header_row,
    } = t;
    let p = off_thread(move || parse_table(format, &bytes, sheet.as_deref(), header_row))
        .await?
        .map_err(preview_error)?;
    let rows: Vec<Value> = p
        .sample_rows
        .iter()
        .map(|r| row_object(&p.columns, r))
        .collect();
    Ok(Json(json!({
        "file": file,
        "format": p.format,
        "row_count": p.row_count,
        "columns": p.columns,
        "rows": rows,
        "sheets": p.sheets,
        "sheet": p.sheet,
        "header_row": header_row,
    })))
}

/// One sample row as `{column: cell}`.
fn row_object(columns: &[String], row: &[String]) -> Value {
    let mut m = Map::new();
    for (i, c) in columns.iter().enumerate() {
        m.insert(
            c.clone(),
            json!(row.get(i).map(|s| s.trim()).unwrap_or_default()),
        );
    }
    Value::Object(m)
}

fn parse_table(
    format: TableFormat,
    bytes: &[u8],
    sheet: Option<&str>,
    header_row: bool,
) -> Result<ColumnPreview, PreviewError> {
    match format {
        TableFormat::Csv => tabular::preview_csv(bytes, header_row),
        TableFormat::Xlsx => tabular::preview_xlsx(bytes, sheet, header_row),
    }
}

fn preview_error(e: PreviewError) -> ApiError {
    match e {
        PreviewError::SheetNotFound(_) => {
            ApiError::Unprocessable("sheet_not_found", format!("{e}."))
        }
        PreviewError::TooLarge(_) => ApiError::TooLarge("too_large", format!("{e}.")),
        PreviewError::TooManyColumns | PreviewError::Empty | PreviewError::Unreadable(_) => {
            ApiError::Unprocessable("unreadable_table", format!("{e}."))
        }
    }
}

/// Runs `parse` on the blocking pool; past [`PREVIEW_TIMEOUT`] the request answers 422 `unreadable_table` (the
/// parse itself is bounded by the tabular caps and finishes on its own).
async fn off_thread<T: Send + 'static>(parse: impl FnOnce() -> T + Send + 'static) -> ApiResult<T> {
    match tokio::time::timeout(PREVIEW_TIMEOUT, tokio::task::spawn_blocking(parse)).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(ApiError::Internal(anyhow::anyhow!("preview task: {e}"))),
        Err(_) => Err(ApiError::Unprocessable(
            "unreadable_table",
            format!(
                "The table took longer than {} s to read; send a smaller or simpler file.",
                PREVIEW_TIMEOUT.as_secs()
            ),
        )),
    }
}

/// Multipart: `file` (CSV or XLSX, at most 10 MB), optional `sheet`, `header_row`, and `type` (when sent, it must
/// be `datatable`).
async fn uploaded_table(req: Request) -> ApiResult<Table> {
    let mut mp = Multipart::from_request(req, &())
        .await
        .map_err(|_| sources::bad_body("The body must be multipart/form-data."))?;
    let mut file: Option<(String, TableFormat, Vec<u8>)> = None;
    let mut sheet: Option<String> = None;
    let mut header_row = true;
    while let Some(mut field) = mp.next_field().await.map_err(multipart_error)? {
        let name = field.name().unwrap_or_default().to_string();
        match name.as_str() {
            "file" if file.is_none() => {
                let file_name = field.file_name().unwrap_or_default().trim().to_string();
                let ct = field.content_type().unwrap_or_default().to_string();
                let format = table_format(&Value::Null, Some(file_name.as_str()), &ct)
                    .ok_or_else(unsupported_type)?;
                let mut bytes = Vec::new();
                while let Some(chunk) = field.chunk().await.map_err(multipart_error)? {
                    if bytes.len() + chunk.len() > MAX_PREVIEW_BYTES {
                        return Err(too_large());
                    }
                    bytes.extend_from_slice(&chunk);
                }
                file = Some((file_name, format, bytes));
            }
            "sheet" => {
                let v = field.text().await.map_err(multipart_error)?;
                sheet = Some(v.trim().to_string()).filter(|s| !s.is_empty());
            }
            "header_row" => {
                let v = field.text().await.map_err(multipart_error)?;
                header_row = header_row_of(&v)?;
            }
            "type" => {
                let v = field.text().await.map_err(multipart_error)?;
                if v.trim() != "datatable" {
                    return Err(not_a_table());
                }
            }
            _ => {}
        }
    }
    let (file, format, bytes) =
        file.ok_or_else(|| invalid("Attach a CSV or XLSX file in the file field."))?;
    Ok(Table {
        file,
        format,
        bytes,
        sheet,
        header_row,
    })
}

/// JSON: the draft datatable source; its table is fetched from `config.url` (netguard, the typed key per
/// `config.auth`, at most 10 MB).
async fn fetched_table(st: &AppState, ctx: &JobCtx, req: Request) -> ApiResult<Table> {
    let bytes = to_bytes(req.into_body(), PREVIEW_JSON_LIMIT)
        .await
        .map_err(|_| sources::bad_body("The draft is not a JSON body of at most 64 KB."))?;
    let raw: Value = serde_json::from_slice(&bytes)
        .map_err(|_| sources::bad_body("The request body must be JSON or multipart."))?;
    let (o, change) = draft_of(raw)?;
    if o.get("type").and_then(Value::as_str) != Some("datatable") {
        return Err(not_a_table());
    }
    let config = o.get("config").cloned().unwrap_or_else(|| json!({}));
    let raw_url = config["url"]
        .as_str()
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .ok_or_else(|| invalid("Enter the table URL first."))?;
    let url = sources::check_source_url(raw_url, st.connectors.policy(), ctx.t.tenant)?;
    sources::validate_auth(config.get("auth"))?;
    let secret = match change {
        CredentialsChange::Set(c) => Some(c.secret),
        CredentialsChange::Keep | CredentialsChange::Clear => None,
    };
    let sheet = o
        .get("sheet")
        .and_then(Value::as_str)
        .or_else(|| config["sheet"].as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let header_row = match o.get("header_row").or_else(|| config.get("header_row")) {
        None | Some(Value::Null) => true,
        Some(Value::Bool(b)) => *b,
        Some(_) => {
            return Err(sources::bad_body("header_row must be true or false."));
        }
    };
    let file = url
        .path_segments()
        .and_then(|mut s| s.next_back())
        .filter(|s| !s.is_empty())
        .unwrap_or("table")
        .to_string();
    let host = host_of(&url);
    // one byte past the cap tells a 10 MB table from a bigger one
    let got = match fetch(
        st,
        ctx.t.tenant,
        &url,
        &config,
        secret.as_deref(),
        MAX_PREVIEW_BYTES + 1,
    )
    .await
    {
        Ok(f) => f,
        Err(FetchError::Refused(detail)) => {
            return Err(ApiError::Unprocessable("address_refused", detail));
        }
        Err(FetchError::NoAnswer(why)) => {
            return Err(invalid(format!(
                "The table could not be read: {host}: {why}."
            )));
        }
    };
    if !got.status.is_success() {
        return Err(invalid(format!(
            "The table URL answered HTTP {} from {host}.",
            got.status.as_u16()
        )));
    }
    if got.body.len() > MAX_PREVIEW_BYTES {
        return Err(too_large());
    }
    if got.truncated {
        return Err(invalid(format!(
            "The table could not be read to the end from {host}."
        )));
    }
    let format = table_format(&config, Some(file.as_str()), &got.content_type)
        .ok_or_else(unsupported_type)?;
    Ok(Table {
        file,
        format,
        bytes: got.body,
        sheet,
        header_row,
    })
}
