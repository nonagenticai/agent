//! Customer attachments through the edge (SW-9, design §3 "Web chat, customer → staff"; contract "Web chat"). Both
//! routes are EdgeKey-gated like the rest of `/api/internal/channel/*`; the edge has already proven the customer
//! (conversation token + session token) and caps the body.
//!
//! - `POST /api/internal/channel/webchat/attachments` — multipart: text fields `tenant`, `job_id`, `binding_id`,
//!   `conversation`, `session_token`, then the one `file`. The binding is checked like `inbound`
//!   ([`internal::checked_binding`]); the session must be live, of that binding and THAT conversation's own
//!   ([`webchat_sessions::verify_session`]: the same rules as the stream and the history), and the tenant and job come
//!   from the binding and the RLS-scoped session row, never from the file. At most [`PER_HOUR`] uploads per
//!   conversation per hour (counter `wca:conv:<id>`, counted before any decoding so a refused file counts too), then
//!   [`validate::accept`] (magic bytes, bomb limits, re-encode, safe PDF) and [`store::AttachmentStore::put`]
//!   (direction `customer`, created by `customer`, unlinked). 201 [`AttachmentRef`]; 422 `attachment_refused`
//!   `{reason}` (a closed code, safe to show); 413 `attachment_quota` / `too_large`; 429 `rate_limited
//!   {retry_after_s}`; 401 `session_invalid`; 403 `session_blocked`.
//! - `GET /api/internal/channel/attachments/{att}?tenant=&job_id=&binding_id=&conversation=` — the bytes, for web chat
//!   and Telegram. Served only when the attachment belongs to that conversation and the conversation arrived on that
//!   binding; a customer never reads a staff upload that was not sent. Every other case is the uniform 404
//!   `not_found`. The display name travels base64url in `x-attachment-name`.
//!
//! The inbound message route links the ids a customer message carries ([`checked_ids`], [`require_customer_unlinked`],
//! [`link`]). Bytes, names and hashes are data: never in a log line, an audit detail or a metric label.
use std::collections::HashMap;

use axum::Router;
use axum::extract::{DefaultBodyLimit, Multipart, State, multipart::MultipartError};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, X_CONTENT_TYPE_OPTIONS};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use base64::Engine as _;
use serde::Deserialize;
use serde_json::Value;
use sqlx::{PgConnection, Row};
use uuid::Uuid;

use super::attachment_settings;
use super::internal::{self, EdgeKey};
use super::{store, webchat_sessions};
use crate::attachments::store::{
    AttachmentStore, Direction, NewAttachment, PgAttachmentStore, StoredAttachment,
};
use crate::attachments::{AttachmentRef, validate};
use crate::audit::{self, Actor, Event};
use crate::channels::attachments_wire::InboundUploadMeta;
use crate::channels::core::ATTACHMENT_NAME_HEADER;
use crate::channels::webchat::CHANNEL;
use crate::error::{ApiError, ApiPath, ApiQuery, ApiResult};
use crate::state::AppState;

/// The multipart body the upload route reads: the 10 MiB file plus its small fields.
pub const UPLOAD_BODY_LIMIT: usize = 11 * 1024 * 1024;
/// Uploads per conversation per hour (counter key `wca:conv:<conversation id>`).
pub const PER_HOUR: i32 = 10;
/// Attachments one customer message carries.
pub const MAX_PER_MESSAGE: usize = 3;
/// A text field of the upload (ids, a session token, a conversation id) is far below this.
const MAX_FIELD_BYTES: usize = 1024;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/internal/channel/webchat/attachments",
            post(upload).layer(DefaultBodyLimit::max(UPLOAD_BODY_LIMIT)),
        )
        .route(
            "/api/internal/channel/attachments",
            post(upload_generic).layer(DefaultBodyLimit::max(UPLOAD_BODY_LIMIT)),
        )
        .route("/api/internal/channel/attachments/{att}", get(fetch))
}

fn not_found() -> ApiError {
    ApiError::Missing("not_found", "not found".into())
}

fn bad_body(msg: &str) -> ApiError {
    ApiError::BadRequest("invalid_body", msg.to_string())
}

fn multipart_error(e: MultipartError) -> ApiError {
    if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
        ApiError::TooLarge("too_large", "the attachment is too large".into())
    } else {
        bad_body("The multipart body is not valid.")
    }
}

/// 422 `attachment_refused` with its closed `reason`.
fn refused(reason: &'static str) -> ApiError {
    ApiError::detailed(
        StatusCode::UNPROCESSABLE_ENTITY,
        "attachment_refused",
        "The file cannot be accepted.",
    )
    .with("reason", reason)
}

/// The fields of an upload.
#[derive(Default)]
struct Fields {
    tenant: Option<Uuid>,
    job_id: Option<Uuid>,
    binding_id: Option<Uuid>,
    conversation: Option<String>,
    session_token: Option<String>,
    file: Option<(String, Vec<u8>)>,
}

async fn read_fields(mut mp: Multipart) -> ApiResult<Fields> {
    let mut f = Fields::default();
    while let Some(field) = mp.next_field().await.map_err(multipart_error)? {
        let name = field.name().unwrap_or_default().to_string();
        if name == "file" {
            if f.file.is_some() {
                return Err(bad_body("Send one file."));
            }
            let declared = field.file_name().unwrap_or_default().to_string();
            let bytes = field.bytes().await.map_err(multipart_error)?;
            f.file = Some((declared, bytes.to_vec()));
            continue;
        }
        if !matches!(
            name.as_str(),
            "tenant" | "job_id" | "binding_id" | "conversation" | "session_token"
        ) {
            continue;
        }
        let text = field.text().await.map_err(multipart_error)?;
        if text.len() > MAX_FIELD_BYTES {
            return Err(bad_body("A field is too long."));
        }
        let id = || Uuid::parse_str(text.trim()).map_err(|_| bad_body("An id is not valid."));
        match name.as_str() {
            "tenant" => f.tenant = Some(id()?),
            "job_id" => f.job_id = Some(id()?),
            "binding_id" => f.binding_id = Some(id()?),
            "conversation" => f.conversation = Some(text),
            _ => f.session_token = Some(text),
        }
    }
    Ok(f)
}

/// `POST /api/internal/channel/webchat/attachments` (see the module doc).
async fn upload(
    _k: EdgeKey,
    State(st): State<AppState>,
    mp: Multipart,
) -> ApiResult<(StatusCode, Json<AttachmentRef>)> {
    let f = read_fields(mp).await?;
    let (Some(tenant), Some(job_id), Some(binding_id), Some(conversation), Some(token), Some(file)) = (
        f.tenant,
        f.job_id,
        f.binding_id,
        f.conversation,
        f.session_token,
        f.file,
    ) else {
        return Err(bad_body(
            "tenant, job_id, binding_id, conversation, session_token and file are required.",
        ));
    };
    if conversation.trim().is_empty() || conversation.chars().any(char::is_control) {
        return Err(bad_body("The conversation is not valid."));
    }
    let job_key =
        internal::checked_binding(&st, binding_id, CHANNEL, "customer", tenant, job_id, None)
            .await?;
    // the admin's setting on this binding: with customer files off nothing is stored
    if !attachment_settings::for_edge_binding(&st.db, binding_id)
        .await?
        .is_some_and(|s| s.customer)
    {
        return Err(attachment_settings::customer_off());
    }
    {
        let mut tx = store::begin_job(&st.db, tenant, job_id).await?;
        webchat_sessions::verify_session(&mut tx, binding_id, &conversation, &token).await?;
        tx.commit().await?;
    }
    let (declared, bytes) = file;
    let meta = store_customer_upload(
        &st,
        tenant,
        job_id,
        &job_key,
        &conversation,
        declared,
        bytes,
    )
    .await?;
    Ok((StatusCode::CREATED, Json(meta)))
}

/// The one path of a customer file into the store, whatever the channel: the per-conversation limit (counted before
/// any decoding, so a refused file counts too), [`validate::accept`], [`store::AttachmentStore::put`] (direction
/// `customer`, unlinked) and the audit row. The caller has proven the customer and resolved `conversation_id`
/// (a conversation of `tenant`/`job_id`).
async fn store_customer_upload(
    st: &AppState,
    tenant: Uuid,
    job_id: Uuid,
    job_key: &str,
    conversation: &str,
    declared: String,
    bytes: Vec<u8>,
) -> ApiResult<AttachmentRef> {
    if let Some(retry) =
        webchat_sessions::hit(&st.db, &format!("wca:conv:{conversation}"), 3600, PER_HOUR).await?
    {
        return Err(ApiError::detailed(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "Too many attachments; try again later.",
        )
        .with("retry_after_s", retry));
    }
    let accepted = tokio::task::spawn_blocking(move || validate::accept(&bytes, &declared))
        .await
        .map_err(|e| ApiError::Internal(anyhow::anyhow!("attachment validation task: {e}")))?
        .map_err(|r| refused(r.0))?;
    let mut tx = store::begin_job(&st.db, tenant, job_id).await?;
    let store = PgAttachmentStore::for_state(st);
    let meta = store
        .put(
            &mut tx,
            NewAttachment {
                conversation_id: conversation,
                direction: Direction::Customer,
                created_by: "customer",
                accepted: &accepted,
            },
        )
        .await?;
    // the bytes are already stored: when the audit row or the commit fails, no row will name them, so delete them
    let finished: ApiResult<()> = async {
        audit::record(
            &mut tx,
            &Actor::system("service:agent-edge"),
            Event::new(
                "conversation_attachment.upload",
                "conversation_attachment",
                meta.id.to_string(),
            )
            .job(job_key)
            .detail("direction", "customer")
            .detail("media_type", meta.media_type.clone())
            .detail("byte_size", meta.byte_size),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }
    .await;
    if let Err(e) = finished {
        store.abandon(tenant, meta.artifact_id).await;
        return Err(e);
    }
    metrics::counter!("agent_attachments_stored_total", "direction" => "customer").increment(1);
    Ok(AttachmentRef::from(&meta))
}

/// `POST /api/internal/channel/attachments` (EdgeKey): multipart `meta` (JSON, [`InboundUploadMeta`], the one definition: `crate::channels::attachments_wire`) + `file`, for a
/// channel whose customer has no web chat session (Telegram). Tenant and job come from the binding row; the
/// conversation is the customer ref `(binding's kind, side customer, conversation_external)` that arrived on that
/// binding. The table keys every file to a stored conversation, so a conversation that does not exist yet is 404
/// `conversation_unknown` (the edge first stores the message with `accept_only`, then uploads, then sends the
/// message again with `attachments`: the resend links them). Same answers as the web chat upload.
async fn upload_generic(
    _k: EdgeKey,
    State(st): State<AppState>,
    mut mp: Multipart,
) -> ApiResult<(StatusCode, Json<AttachmentRef>)> {
    let (mut meta, mut file) = (None::<InboundUploadMeta>, None::<Vec<u8>>);
    while let Some(field) = mp.next_field().await.map_err(multipart_error)? {
        match field.name().unwrap_or_default() {
            "meta" => {
                let t = field.text().await.map_err(multipart_error)?;
                if t.len() > 4 * MAX_FIELD_BYTES {
                    return Err(bad_body("meta is too long."));
                }
                meta = Some(serde_json::from_str(&t).map_err(|_| bad_body("meta is not valid."))?);
            }
            "file" if file.is_none() => {
                file = Some(field.bytes().await.map_err(multipart_error)?.to_vec());
            }
            _ => {}
        }
    }
    let (Some(meta), Some(bytes)) = (meta, file) else {
        return Err(bad_body("meta and file are required."));
    };
    let conversation_external = meta.conversation.id.clone();
    if conversation_external.trim().is_empty()
        || conversation_external.chars().any(char::is_control)
    {
        return Err(bad_body("The conversation is not valid."));
    }
    let row = sqlx::query(
        "SELECT job_key, kind, side, tenant_id, job_id, settings FROM agent_connector_bindings() WHERE id = $1",
    )
    .bind(meta.binding_id)
    .fetch_optional(&st.db)
    .await?
    .filter(|r| {
        r.get::<String, _>("side") == "customer"
            && r.get::<Uuid, _>("tenant_id") == meta.tenant
            && r.get::<Uuid, _>("job_id") == meta.job_id
            && r.get::<String, _>("kind") == meta.kind
    })
    .ok_or_else(not_found)?;
    let (tenant, job_id): (Uuid, Uuid) = (row.get("tenant_id"), row.get("job_id"));
    let (job_key, kind): (String, String) = (row.get("job_key"), row.get("kind"));
    // the admin's setting on the binding: with customer files off (or a kind without file support) nothing is stored
    if !attachment_settings::Switches::of(&kind, &row.get::<Value, _>("settings")).customer {
        return Err(attachment_settings::customer_off());
    }
    let mut tx = store::begin_job(&st.db, tenant, job_id).await?;
    let conversation: Option<String> = sqlx::query_scalar(
        "SELECT conversation_id FROM channel_conversation_refs \
          WHERE channel = $1 AND side = 'customer' AND external_id = $2 AND route->>'binding_id' = $3",
    )
    .bind(&kind)
    .bind(&conversation_external)
    .bind(meta.binding_id.to_string())
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    let conversation = conversation.ok_or_else(|| {
        ApiError::Missing("conversation_unknown", "no such conversation yet".into())
    })?;
    let r = store_customer_upload(
        &st,
        tenant,
        job_id,
        &job_key,
        &conversation,
        meta.declared_name,
        bytes,
    )
    .await?;
    Ok((StatusCode::CREATED, Json(r)))
}

#[derive(Deserialize)]
struct FetchQuery {
    tenant: Uuid,
    job_id: Uuid,
    /// Web chat names its binding (checked, and the conversation must have arrived on it); Telegram's edge names
    /// the channel's conversation id from the deliver request instead and core resolves it in the job.
    #[serde(default)]
    binding_id: Option<Uuid>,
    conversation: String,
}

/// `GET /api/internal/channel/attachments/{att}` (see the module doc).
async fn fetch(
    _k: EdgeKey,
    State(st): State<AppState>,
    ApiPath(att): ApiPath<String>,
    ApiQuery(q): ApiQuery<FetchQuery>,
) -> ApiResult<Response> {
    let att = Uuid::parse_str(&att).map_err(|_| not_found())?;
    if let Some(binding_id) = q.binding_id {
        let kind: Option<String> =
            sqlx::query_scalar("SELECT kind FROM agent_connector_bindings() WHERE id = $1")
                .bind(binding_id)
                .fetch_optional(&st.db)
                .await?;
        let kind = kind.ok_or_else(not_found)?;
        internal::checked_binding(&st, binding_id, &kind, "customer", q.tenant, q.job_id, None)
            .await
            .map_err(|_| not_found())?;
    }
    let mut tx = store::begin_job(&st.db, q.tenant, q.job_id).await?;
    let conversation = match q.binding_id {
        // the conversation must have arrived on this binding
        Some(binding_id) => {
            let on_binding: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM channel_conversation_refs \
                                 WHERE conversation_id = $1 AND side = 'customer' AND route->>'binding_id' = $2)",
            )
            .bind(&q.conversation)
            .bind(binding_id.to_string())
            .fetch_one(&mut *tx)
            .await?;
            if !on_binding {
                return Err(not_found());
            }
            q.conversation.clone()
        }
        // a core conversation id, or the channel's own id for one customer conversation of this job
        None => {
            let ids: Vec<String> = sqlx::query_scalar(
                "SELECT DISTINCT conversation_id FROM channel_conversation_refs \
                  WHERE side = 'customer' AND (conversation_id = $1 OR external_id = $1) LIMIT 2",
            )
            .bind(&q.conversation)
            .fetch_all(&mut *tx)
            .await?;
            match ids.as_slice() {
                [one] => one.clone(),
                _ => return Err(not_found()),
            }
        }
    };
    let found = PgAttachmentStore::for_state(&st)
        .get(&mut tx, &conversation, att)
        .await?;
    tx.commit().await?;
    // a staff file the customer was not sent (still a draft in the composer) is not theirs to read
    let StoredAttachment { meta, bytes } = found
        .filter(|a| a.meta.direction == Direction::Customer || a.meta.message_id.is_some())
        .ok_or_else(not_found)?;
    let mut r = (StatusCode::OK, bytes).into_response();
    let h = r.headers_mut();
    if let Ok(v) = HeaderValue::from_str(&meta.media_type) {
        h.insert(CONTENT_TYPE, v);
    }
    let name =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(meta.display_name.as_bytes());
    if let Ok(v) = HeaderValue::from_str(&name) {
        h.insert(ATTACHMENT_NAME_HEADER, v);
    }
    h.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
    Ok(r)
}

// ---- the inbound message (conversations::internal) ----------------------------------------------------------------

/// The attachment ids of an inbound message: deduplicated, at most [`MAX_PER_MESSAGE`], and only on a channel whose
/// customers can attach (422 `invalid_attachments` / `too_many_attachments`).
pub(super) fn checked_ids(kind: &str, ids: &[Uuid]) -> ApiResult<Vec<Uuid>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    if !attachment_settings::kind_takes_files(kind) {
        return Err(ApiError::Unprocessable(
            "invalid_attachments",
            "this channel takes no attachments".into(),
        ));
    }
    let mut unique = ids.to_vec();
    unique.sort_unstable();
    unique.dedup();
    if unique.len() > MAX_PER_MESSAGE {
        return Err(ApiError::Unprocessable(
            "too_many_attachments",
            format!("a message carries at most {MAX_PER_MESSAGE} attachments"),
        ));
    }
    Ok(unique)
}

/// 404 (the uniform `not_found`) unless every id is a live, unlinked CUSTOMER attachment of `conversation_id`.
pub(super) async fn require_customer_unlinked(
    conn: &mut PgConnection,
    conversation_id: &str,
    ids: &[Uuid],
) -> ApiResult<()> {
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM conversation_attachment \
          WHERE id = ANY($1) AND conversation_id = $2 AND tenant_id = app_current_tenant() \
            AND direction = 'customer' AND message_id IS NULL AND deleted_at IS NULL",
    )
    .bind(ids)
    .bind(conversation_id)
    .fetch_one(conn)
    .await?;
    if usize::try_from(n).ok() == Some(ids.len()) {
        Ok(())
    } else {
        Err(not_found())
    }
}

/// Link `ids` (checked by [`require_customer_unlinked`] in the same transaction) to the stored message.
pub(super) async fn link(
    conn: &mut PgConnection,
    conversation_id: &str,
    message_id: Uuid,
    ids: &[Uuid],
) -> ApiResult<()> {
    if ids.is_empty() {
        return Ok(());
    }
    require_customer_unlinked(&mut *conn, conversation_id, ids).await?;
    crate::attachments::store::link_to_message(conn, conversation_id, message_id, ids).await
}

/// [`link`] in a transaction of its own, for a message the runtime stored (its transaction is not ours).
pub(super) async fn link_stored(
    st: &AppState,
    tenant: Uuid,
    job_id: Uuid,
    conversation_id: &str,
    message_id: Uuid,
    ids: &[Uuid],
) -> ApiResult<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let mut tx = store::begin_job(&st.db, tenant, job_id).await?;
    link(&mut tx, conversation_id, message_id, ids).await?;
    tx.commit().await?;
    Ok(())
}

/// The refs of the attachments linked to each of `message_ids` (a message of `conversation_id`), oldest first. A
/// message without files has no entry.
pub async fn refs_by_message(
    conn: &mut PgConnection,
    conversation_id: &str,
    message_ids: &[Uuid],
) -> ApiResult<HashMap<Uuid, Vec<AttachmentRef>>> {
    let mut out: HashMap<Uuid, Vec<AttachmentRef>> = HashMap::new();
    if message_ids.is_empty() {
        return Ok(out);
    }
    let rows = sqlx::query(
        "SELECT id, message_id, media_type, display_name, byte_size, width, height FROM conversation_attachment \
          WHERE conversation_id = $1 AND message_id = ANY($2) AND tenant_id = app_current_tenant() \
            AND deleted_at IS NULL \
          ORDER BY created_at, id",
    )
    .bind(conversation_id)
    .bind(message_ids)
    .fetch_all(conn)
    .await?;
    for r in rows {
        let message_id: Uuid = r.try_get("message_id")?;
        out.entry(message_id).or_default().push(AttachmentRef {
            id: r.try_get("id")?,
            media_type: r.try_get("media_type")?,
            display_name: r.try_get("display_name")?,
            byte_size: r.try_get("byte_size")?,
            width: r.try_get("width")?,
            height: r.try_get("height")?,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_deduplicated_capped_and_only_for_channels_that_attach() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        assert_eq!(checked_ids("web_chat", &[]).unwrap(), Vec::<Uuid>::new());
        assert_eq!(checked_ids("web_chat", &[a, a]).unwrap(), vec![a]);
        let four: Vec<Uuid> = (0..4).map(|_| Uuid::new_v4()).collect();
        assert_eq!(
            checked_ids("web_chat", &four).unwrap_err().code(),
            "too_many_attachments"
        );
        // voice carries no files; telegram does
        assert_eq!(
            checked_ids("voice", &[b]).unwrap_err().code(),
            "invalid_attachments"
        );
        assert!(checked_ids("telegram", &[b]).is_ok());
    }

    #[test]
    fn a_refusal_names_a_closed_reason() {
        let e = refused(validate::reason::UNSUPPORTED_TYPE);
        assert_eq!(e.code(), "attachment_refused");
    }
}
