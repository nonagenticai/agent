//! `/api/jobs/{job}/discovery`: J18 discovery (B8 T14a, T15a, T15 shadow half; plan "Route table"). Shapes = web
//! `types/journey.ts` (`DiscoveryState`, `DiscoveryImport`, `ShadowMode`); the logic is crate::discovery.
//!
//! - `GET …/discovery` (member) → `DiscoveryState`.
//! - `POST …/discovery/imports` (job_designer; multipart `file`, `text_column?`, `count_column?`; 20 MB) →
//!   `DiscoveryState` with the new import `classifying`. 422 `unsupported_file`, `empty_file`, `unknown_column`,
//!   `count_not_numeric`; 413 `too_large`. Audit `discovery.import_upload` (format and counts; no file text).
//! - `POST …/discovery/imports/preview` (job_designer; multipart `file`) → `{format, columns, sample_rows, rows,
//!   suggested_text_column, suggested_count_column}`; stores nothing, calls no model. As upload's 422/413.
//! - `DELETE …/discovery/imports/{id}` (job_designer) → `DiscoveryState`; 404 `import_not_found`. A classifying
//!   import's worker stops at its next batch. Audit `discovery.import_delete`.
//! - `PUT …/discovery/scorecard/{activity}` `{weight}` (job_designer) → `DiscoveryState`; 404 `activity_not_found`,
//!   422 `invalid_weight`. Audit `discovery.weight_set`.
//! - `POST …/discovery/shadow` `{env}` (job_designer) → `DiscoveryState`; 409 `shadow_active`, 422 `invalid_env`.
//!   `DELETE …/discovery/shadow` → `DiscoveryState`; 409 `shadow_inactive`. Audit `discovery.shadow_start|stop`.
//! - `POST …/discovery/proposals/{id}/accept` (job_designer) → `DiscoveryState`; the D54 draft write, live untouched.
//!   404 `proposal_not_found`; 409 `already_decided`, `already_in_job`, `kind_not_in_job`, `no_routing`,
//!   `no_route_row`; 422 `unknown_store`, `invalid_key`. Audit `discovery.proposal_accept`.
//! - `POST …/discovery/proposals/{id}/reject` `{reason}` (job_designer) → `DiscoveryState`. 404 `proposal_not_found`;
//!   409 `already_decided`; 422 `reason_required`. Audit `discovery.proposal_reject`.
use std::time::Duration;

use axum::extract::multipart::MultipartError;
use axum::extract::{DefaultBodyLimit, FromRequest, Multipart, Request, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::Value;

use crate::audit::{self, Event};
use crate::design::{self, JobRef};
use crate::discovery::rows::{self, Format};
use crate::discovery::worker::{self, ImportRef};
use crate::discovery::{import, proposals, shadow, state};
use crate::error::{ApiError, ApiJson, ApiPath, ApiResult};
use crate::state::AppState;
use crate::tenancy::{JobMember, JobRequires, role};

/// Largest export an import or a preview reads.
pub const MAX_UPLOAD_BYTES: usize = 20 * 1024 * 1024;
/// The upload routes' body limit: the file plus its form fields.
const UPLOAD_BODY_LIMIT: usize = MAX_UPLOAD_BYTES + 64 * 1024;
/// Longest a file may take to parse before the request answers 422 `unsupported_file`.
const PARSE_TIMEOUT: Duration = Duration::from_secs(30);

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/jobs/{job}/discovery", get(read))
        .route(
            "/api/jobs/{job}/discovery/imports",
            post(upload).layer(DefaultBodyLimit::max(UPLOAD_BODY_LIMIT)),
        )
        // static segments win over `{id}` in axum's router
        .route(
            "/api/jobs/{job}/discovery/imports/preview",
            post(preview).layer(DefaultBodyLimit::max(UPLOAD_BODY_LIMIT)),
        )
        .route("/api/jobs/{job}/discovery/imports/{id}", delete(remove))
        .route(
            "/api/jobs/{job}/discovery/scorecard/{activity}",
            put(set_weight),
        )
        .route(
            "/api/jobs/{job}/discovery/proposals/{id}/accept",
            post(accept),
        )
        .route(
            "/api/jobs/{job}/discovery/proposals/{id}/reject",
            post(reject),
        )
        .route(
            "/api/jobs/{job}/discovery/shadow",
            post(shadow_start).delete(shadow_stop),
        )
}

/// `{id}` under `/api/jobs/{job}` (serde ignores the `job` key).
#[derive(Deserialize)]
struct ImportPath {
    id: String,
}

/// `{activity}` under `/api/jobs/{job}`.
#[derive(Deserialize)]
struct ActivityPath {
    activity: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ShadowBody {
    env: String,
}

/// `discoveryState`.
async fn read(JobMember(ctx): JobMember, State(st): State<AppState>) -> ApiResult<Json<Value>> {
    let mut tx = ctx.begin(&st.db).await?;
    let v = state::view(&mut tx, &st, &ctx).await?;
    tx.commit().await?;
    Ok(Json(v))
}

/// An uploaded export and the columns chosen for it.
struct Upload {
    file_name: String,
    format: Format,
    bytes: Vec<u8>,
    text_column: Option<String>,
    count_column: Option<String>,
}

fn bad_body(msg: &str) -> ApiError {
    ApiError::BadRequest("invalid_body", msg.to_string())
}

/// A body over the route's limit is too large; any other parse failure is a malformed body.
fn multipart_error(e: MultipartError) -> ApiError {
    if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
        rows::too_large()
    } else {
        bad_body("The multipart body is not valid.")
    }
}

/// A form field naming a column: blank = not chosen.
fn column_field(v: &str) -> Option<String> {
    let t = v.trim();
    (!t.is_empty()).then(|| t.to_string())
}

/// Multipart `file` (`.csv`, `.json`, `.xlsx`; at most 20 MB), `text_column?`, `count_column?`. The format is
/// refused (422 `unsupported_file`) before the file is read.
async fn read_upload(req: Request) -> ApiResult<Upload> {
    let mut mp = Multipart::from_request(req, &())
        .await
        .map_err(|_| bad_body("The body must be multipart/form-data with a file field."))?;
    let mut file: Option<(String, Format, Vec<u8>)> = None;
    let mut text_column = None;
    let mut count_column = None;
    while let Some(mut field) = mp.next_field().await.map_err(multipart_error)? {
        let name = field.name().unwrap_or_default().to_string();
        match name.as_str() {
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
                let v = field.text().await.map_err(multipart_error)?;
                text_column = column_field(&v);
            }
            "count_column" => {
                let v = field.text().await.map_err(multipart_error)?;
                count_column = column_field(&v);
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

/// Runs `parse` on the blocking pool; past [`PARSE_TIMEOUT`] the request answers 422 `unsupported_file`.
async fn off_thread<T: Send + 'static>(
    parse: impl FnOnce() -> ApiResult<T> + Send + 'static,
) -> ApiResult<T> {
    match tokio::time::timeout(PARSE_TIMEOUT, tokio::task::spawn_blocking(parse)).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => Err(ApiError::Internal(anyhow::anyhow!(
            "discovery parse task: {e}"
        ))),
        Err(_) => Err(ApiError::Unprocessable(
            "unsupported_file",
            format!(
                "The file took longer than {} s to read; send a smaller or simpler export.",
                PARSE_TIMEOUT.as_secs()
            ),
        )),
    }
}

/// `uploadDiscoveryImport`.
async fn upload(
    JobRequires(ctx, _): JobRequires<role::JobDesigner>,
    State(st): State<AppState>,
    req: Request,
) -> ApiResult<Json<Value>> {
    let up = read_upload(req).await?;
    let file = import::file_label(&up.file_name);
    let format = up.format;
    let (bytes, text_column, count_column) = (up.bytes, up.text_column, up.count_column);
    let got = off_thread(move || {
        let t = rows::parse(format, &bytes)?;
        rows::collect(&t, text_column.as_deref(), count_column.as_deref())
    })
    .await?;
    let mut tx = ctx.begin(&st.db).await?;
    let id = import::insert(&mut tx, &ctx, &file, format, &got).await?;
    audit::record(
        &mut tx,
        &ctx.t.actor(),
        Event::new("discovery.import_upload", "discovery_import", id.clone())
            .job(&ctx.job_key)
            .detail("format", format.as_str())
            .detail("rows", got.rows)
            .detail("distinct_rows", got.distinct.len())
            .detail("count_column", got.count_column.is_some()),
    )
    .await?;
    let v = state::view(&mut tx, &st, &ctx).await?;
    tx.commit().await?;
    drop(worker::spawn(
        st.db.clone(),
        worker::classifier(&st),
        ImportRef {
            tenant: ctx.t.tenant,
            job_id: ctx.job_id,
            id,
        },
    ));
    Ok(Json(v))
}

/// `previewDiscoveryImport`: nothing stored, no model call.
async fn preview(
    JobRequires(_ctx, _): JobRequires<role::JobDesigner>,
    req: Request,
) -> ApiResult<Json<Value>> {
    let up = read_upload(req).await?;
    let (format, bytes) = (up.format, up.bytes);
    let v = off_thread(move || rows::parse(format, &bytes).map(|t| rows::preview(&t))).await?;
    Ok(Json(v))
}

/// `deleteDiscoveryImport`.
async fn remove(
    JobRequires(ctx, _): JobRequires<role::JobDesigner>,
    State(st): State<AppState>,
    ApiPath(ImportPath { id }): ApiPath<ImportPath>,
) -> ApiResult<Json<Value>> {
    let mut tx = ctx.begin(&st.db).await?;
    let was = import::delete(&mut tx, &ctx, &id).await?;
    audit::record(
        &mut tx,
        &ctx.t.actor(),
        Event::new("discovery.import_delete", "discovery_import", id)
            .job(&ctx.job_key)
            .detail("status", was),
    )
    .await?;
    let v = state::view(&mut tx, &st, &ctx).await?;
    tx.commit().await?;
    Ok(Json(v))
}

/// `setScorecardWeight`.
async fn set_weight(
    JobRequires(ctx, _): JobRequires<role::JobDesigner>,
    State(st): State<AppState>,
    ApiPath(ActivityPath { activity }): ApiPath<ActivityPath>,
    ApiJson(body): ApiJson<Value>,
) -> ApiResult<Json<Value>> {
    let weight = state::weight_of(&body)?;
    let mut tx = ctx.begin(&st.db).await?;
    state::set_weight(&mut tx, &st, &ctx, &activity, weight).await?;
    audit::record(
        &mut tx,
        &ctx.t.actor(),
        Event::new("discovery.weight_set", "discovery_weight", activity)
            .job(&ctx.job_key)
            .detail("weight", weight),
    )
    .await?;
    let v = state::view(&mut tx, &st, &ctx).await?;
    tx.commit().await?;
    Ok(Json(v))
}

/// `startShadowMode`.
async fn shadow_start(
    JobRequires(ctx, _): JobRequires<role::JobDesigner>,
    State(st): State<AppState>,
    ApiJson(body): ApiJson<ShadowBody>,
) -> ApiResult<Json<Value>> {
    let env = shadow::env_of(&body.env)?;
    let mut tx = ctx.begin(&st.db).await?;
    shadow::start(&mut tx, &ctx, env).await?;
    audit::record(
        &mut tx,
        &ctx.t.actor(),
        Event::new("discovery.shadow_start", "shadow_mode", ctx.job_key.clone())
            .job(&ctx.job_key)
            .detail("env", env),
    )
    .await?;
    let v = state::view(&mut tx, &st, &ctx).await?;
    tx.commit().await?;
    Ok(Json(v))
}

/// `stopShadowMode`.
async fn shadow_stop(
    JobRequires(ctx, _): JobRequires<role::JobDesigner>,
    State(st): State<AppState>,
) -> ApiResult<Json<Value>> {
    let mut tx = ctx.begin(&st.db).await?;
    let env = shadow::stop(&mut tx, &ctx).await?;
    audit::record(
        &mut tx,
        &ctx.t.actor(),
        Event::new("discovery.shadow_stop", "shadow_mode", ctx.job_key.clone())
            .job(&ctx.job_key)
            .detail("env", env),
    )
    .await?;
    let v = state::view(&mut tx, &st, &ctx).await?;
    tx.commit().await?;
    Ok(Json(v))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RejectBody {
    #[serde(default)]
    reason: String,
}

/// `acceptDiscoveryProposal`.
async fn accept(
    JobRequires(ctx, _): JobRequires<role::JobDesigner>,
    State(st): State<AppState>,
    ApiPath(ImportPath { id }): ApiPath<ImportPath>,
) -> ApiResult<Json<Value>> {
    let mut tx = ctx.begin(&st.db).await?;
    let jr = JobRef {
        id: ctx.job_id,
        key: ctx.job_key.clone(),
    };
    design::ensure_job(&mut tx, &st.design, &jr).await?;
    // serialises concurrent decisions on the job (the decision row's key is the second guard)
    sqlx::query("SELECT 1 FROM jobs WHERE id = $1 FOR UPDATE")
        .bind(ctx.job_id)
        .execute(&mut *tx)
        .await?;
    let (kind, keys) = proposals::accept(&mut tx, &st, &ctx, &id).await?;
    audit::record(
        &mut tx,
        &ctx.t.actor(),
        Event::new("discovery.proposal_accept", "discovery_proposal", id)
            .job(&ctx.job_key)
            .detail("kind", kind)
            .detail("keys", keys.len()),
    )
    .await?;
    let v = state::view(&mut tx, &st, &ctx).await?;
    tx.commit().await?;
    Ok(Json(v))
}

/// `rejectDiscoveryProposal`.
async fn reject(
    JobRequires(ctx, _): JobRequires<role::JobDesigner>,
    State(st): State<AppState>,
    ApiPath(ImportPath { id }): ApiPath<ImportPath>,
    ApiJson(body): ApiJson<RejectBody>,
) -> ApiResult<Json<Value>> {
    let mut tx = ctx.begin(&st.db).await?;
    sqlx::query("SELECT 1 FROM jobs WHERE id = $1 FOR UPDATE")
        .bind(ctx.job_id)
        .execute(&mut *tx)
        .await?;
    let kind = proposals::reject(&mut tx, &st, &ctx, &id, &body.reason).await?;
    audit::record(
        &mut tx,
        &ctx.t.actor(),
        Event::new("discovery.proposal_reject", "discovery_proposal", id)
            .job(&ctx.job_key)
            .detail("kind", kind)
            .detail("reason_chars", body.reason.trim().chars().count()),
    )
    .await?;
    let v = state::view(&mut tx, &st, &ctx).await?;
    tx.commit().await?;
    Ok(Json(v))
}
