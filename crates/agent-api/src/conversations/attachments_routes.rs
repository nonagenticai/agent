//! Staff attachments of a conversation (SW-9, design §4/§5, contract "Staff API"):
//!
//! - `POST /api/jobs/{job}/conversations/{id}/attachments`: multipart field `file` (one file, body at most 11 MiB),
//!   `support_agent` or `support_manager` (a conversation worker). Validated and neutralised by
//!   [`crate::attachments::validate::accept`] on the blocking pool, stored unlinked (direction `staff`) → 201
//!   `AttachmentRef`. 422 `attachment_refused` `{reason}`, 413 `too_large` / `attachment_quota`.
//! - `GET …/attachments/{att}`: any member of the job; the bytes with `Content-Type` from the stored media type,
//!   `nosniff`, `private, no-store`, a sandboxing CSP and `Content-Disposition` (inline for an image, attachment for a
//!   PDF; the sanitised name RFC 5987 encoded).
//! - `DELETE …/attachments/{att}`: a conversation worker; an unsent upload only → 204, a sent one is 409
//!   `attachment_sent`.
//!
//! Another tenant's, another conversation's, a soft-deleted and a malformed id all answer the same 404. Bytes are only
//! ever read through [`AttachmentStore::get`]. The audit rows (`conversation_attachment.upload` / `.download_staff`)
//! carry the attachment id, media type and size: never a name, never content. The reply that sends attachments is the
//! Send route (crate::conversations::staff).
use axum::body::Body;
use axum::extract::{FromRequest, Multipart, Request, State, multipart::MultipartError};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use axum::{Router, extract::DefaultBodyLimit};
use sqlx::Row;
use uuid::Uuid;

use super::store;
use crate::attachments::AttachmentRef;
use crate::attachments::store::{AttachmentStore, Direction, NewAttachment, PgAttachmentStore};
use crate::attachments::validate::{self, reason};
use crate::audit::{self, Event};
use crate::error::{ApiError, ApiPath, ApiResult};
use crate::state::AppState;
use crate::tenancy::JobMember;

/// The request body limit: the 10 MiB file plus the multipart framing.
pub const BODY_LIMIT: usize = 11 * 1024 * 1024;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/jobs/{job}/conversations/{id}/attachments",
            post(upload_attachment).layer(DefaultBodyLimit::max(BODY_LIMIT)),
        )
        .route(
            "/api/jobs/{job}/conversations/{id}/attachments/{att}",
            get(download_attachment).delete(delete_attachment),
        )
}

fn not_found() -> ApiError {
    ApiError::Missing("not_found", "not found".into())
}

fn too_large() -> ApiError {
    ApiError::TooLarge("too_large", "The file is too large: at most 10 MB.".into())
}

fn bad_body(msg: &str) -> ApiError {
    ApiError::BadRequest("invalid_body", msg.to_string())
}

fn multipart_error(e: MultipartError) -> ApiError {
    if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
        too_large()
    } else {
        bad_body("The multipart body is not valid.")
    }
}

/// What the person may be told about a refusal (the `reason` is the machine code).
fn refusal_message(r: &str) -> &'static str {
    match r {
        reason::UNSUPPORTED_TYPE => "Only JPEG, PNG, WebP images and PDF files can be attached.",
        reason::TOO_MANY_PIXELS => "The image is too large: at most 4096 pixels on a side.",
        reason::ANIMATED => "Animated images cannot be attached.",
        reason::MALFORMED_IMAGE => "The image could not be read.",
        reason::MALFORMED_PDF => "The PDF could not be read.",
        reason::ACTIVE_PDF => "The PDF contains active content and cannot be attached.",
        reason::ENCRYPTED_PDF => "Encrypted PDFs cannot be attached.",
        _ => "This file cannot be attached.",
    }
}

/// The multipart `file`: (declared name, bytes), at most [`validate::MAX_BYTES`].
async fn read_file(req: Request) -> ApiResult<(String, Vec<u8>)> {
    let mut mp = Multipart::from_request(req, &())
        .await
        .map_err(|_| bad_body("The body must be multipart/form-data with a file field."))?;
    while let Some(mut field) = mp.next_field().await.map_err(multipart_error)? {
        if field.name() != Some("file") {
            continue;
        }
        let name = field.file_name().unwrap_or_default().to_string();
        let mut bytes: Vec<u8> = Vec::new();
        while let Some(chunk) = field.chunk().await.map_err(multipart_error)? {
            if bytes.len() + chunk.len() > validate::MAX_BYTES {
                return Err(too_large());
            }
            bytes.extend_from_slice(&chunk);
        }
        return Ok((name, bytes));
    }
    Err(bad_body("Attach the file in the file field."))
}

/// `POST …/attachments` (see the module doc).
async fn upload_attachment(
    JobMember(j): JobMember,
    State(st): State<AppState>,
    ApiPath((_, id)): ApiPath<(String, String)>,
    req: Request,
) -> ApiResult<(StatusCode, Json<AttachmentRef>)> {
    j.t.require_conversation_worker()?;
    let conv = {
        let mut tx = j.begin(&st.db).await?;
        let conv = store::conversation_or_404(&mut tx, &id).await?;
        // the admin's setting on the customer's channel binding: with staff files off nothing is stored
        let switches =
            super::attachment_settings::for_conversation(&mut tx, &conv.id, &conv.channel).await?;
        tx.commit().await?;
        if !switches.staff {
            return Err(super::attachment_settings::staff_off());
        }
        conv
    };
    let (name, bytes) = read_file(req).await?;
    let accepted = tokio::task::spawn_blocking(move || validate::accept(&bytes, &name))
        .await
        .map_err(|e| ApiError::Internal(anyhow::anyhow!("attachment validation task: {e}")))?
        .map_err(|r| {
            if r.0 == reason::TOO_LARGE {
                too_large()
            } else {
                ApiError::Unprocessable("attachment_refused", refusal_message(r.0).into())
                    .with("reason", r.0)
            }
        })?;
    let store = PgAttachmentStore::for_state(&st);
    let mut tx = j.begin(&st.db).await?;
    let meta = store
        .put(
            &mut tx,
            NewAttachment {
                conversation_id: &conv.id,
                direction: Direction::Staff,
                created_by: &j.t.caller.sub,
                accepted: &accepted,
            },
        )
        .await?;
    // the bytes are already stored: when the audit row or the commit fails, no row will name them, so delete them
    let finished: ApiResult<()> = async {
        audit::record(
            &mut tx,
            &j.t.actor(),
            Event::new(
                "conversation_attachment.upload",
                "conversation_attachment",
                meta.id.to_string(),
            )
            .job(&j.job_key)
            .detail("media_type", meta.media_type.clone())
            .detail("byte_size", meta.byte_size),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }
    .await;
    if let Err(e) = finished {
        store.abandon(j.t.tenant, meta.artifact_id).await;
        return Err(e);
    }
    metrics::counter!("agent_attachment_uploads_total", "direction" => Direction::Staff.as_str())
        .increment(1);
    Ok((StatusCode::CREATED, Json(AttachmentRef::from(&meta))))
}

/// `filename*=UTF-8''…` (RFC 5987) plus an ASCII `filename=` fallback for `name`.
fn disposition(inline: bool, name: &str) -> String {
    let mut enc = String::new();
    for b in name.bytes() {
        if b.is_ascii_alphanumeric() || b"!#$&+-.^_`|~".contains(&b) {
            enc.push(b as char);
        } else {
            enc.push_str(&format!("%{b:02X}"));
        }
    }
    let ascii: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!(
        "{}; filename=\"{ascii}\"; filename*=UTF-8''{enc}",
        if inline { "inline" } else { "attachment" }
    )
}

/// `GET …/attachments/{att}`.
async fn download_attachment(
    JobMember(j): JobMember,
    State(st): State<AppState>,
    ApiPath((_, id, att)): ApiPath<(String, String, String)>,
) -> ApiResult<Response> {
    let att = Uuid::parse_str(att.trim()).map_err(|_| not_found())?;
    let mut tx = j.begin(&st.db).await?;
    let conv = store::conversation_or_404(&mut tx, &id).await?;
    let found = PgAttachmentStore::for_state(&st)
        .get(&mut tx, &conv.id, att)
        .await?
        .ok_or_else(not_found)?;
    audit::record(
        &mut tx,
        &j.t.actor(),
        Event::new(
            "conversation_attachment.download_staff",
            "conversation_attachment",
            att.to_string(),
        )
        .job(&j.job_key)
        .detail("media_type", found.meta.media_type.clone())
        .detail("byte_size", found.meta.byte_size),
    )
    .await?;
    tx.commit().await?;
    let inline = found.meta.media_type.starts_with("image/");
    let mut resp = Response::new(Body::from(found.bytes));
    let h = resp.headers_mut();
    let hv = |s: &str| HeaderValue::from_str(s).unwrap_or_else(|_| HeaderValue::from_static(""));
    h.insert(header::CONTENT_TYPE, hv(&found.meta.media_type));
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("sandbox; default-src 'none'"),
    );
    h.insert(
        header::CONTENT_DISPOSITION,
        hv(&disposition(inline, &found.meta.display_name)),
    );
    Ok(resp)
}

/// `DELETE …/attachments/{att}`.
async fn delete_attachment(
    JobMember(j): JobMember,
    State(st): State<AppState>,
    ApiPath((_, id, att)): ApiPath<(String, String, String)>,
) -> ApiResult<Response> {
    j.t.require_conversation_worker()?;
    let att = Uuid::parse_str(att.trim()).map_err(|_| not_found())?;
    let mut tx = j.begin(&st.db).await?;
    let conv = store::conversation_or_404(&mut tx, &id).await?;
    if PgAttachmentStore::for_state(&st)
        .delete(&mut tx, &conv.id, att)
        .await?
    {
        tx.commit().await?;
        return Ok(StatusCode::NO_CONTENT.into_response());
    }
    // a sent attachment is part of the transcript (409); anything else is the uniform 404
    let linked: Option<Option<Uuid>> = sqlx::query(
        "SELECT message_id FROM conversation_attachment \
          WHERE id = $1 AND conversation_id = $2 AND tenant_id = app_current_tenant() AND deleted_at IS NULL",
    )
    .bind(att)
    .bind(&conv.id)
    .fetch_optional(&mut *tx)
    .await?
    .map(|r| r.get("message_id"));
    tx.commit().await?;
    match linked {
        Some(Some(_)) => Err(ApiError::Conflict(
            "attachment_sent",
            "This file was already sent.".into(),
        )),
        _ => Err(not_found()),
    }
}
