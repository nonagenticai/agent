//! The attachment store (SW-9 design §1/§5): [`AttachmentStore`] and its implementation over `conversation_attachment`
//! (0205 + 0223, metadata in Postgres) and the platform artifact_service (the bytes, scanned there by ClamAV;
//! [`super::artifacts`]).
//!
//! Every function takes the connection of a transaction opened by [`crate::conversations::store::begin_job`] (tenant
//! and job GUCs set: RLS confines every statement to the job). The tenant and the job are never arguments — they come
//! from the GUCs, never from a request body — and every read and write also names the conversation, so an id from
//! another conversation answers like a missing one (`ApiError::Missing`, the uniform 404).
//!
//! Only [`Accepted`](super::validate::Accepted) values are stored: the caller has already run
//! [`validate::accept`](super::validate::accept). The per-tenant quota is checked here, under a per-tenant advisory
//! lock so two concurrent uploads cannot both squeeze under the line.
//!
//! `put` first commits an upload intent (0223, own transaction), uploads to artifact_service, then inserts the
//! metadata row and deletes the intent in one savepoint: an insert that fails after the upload deletes the artifact
//! right away or, when that fails too, queues its tombstone outside the failed transaction, and a process that dies
//! anywhere in between leaves the intent for the sweep, which finds the artifact by its `att:<id>` tag and deletes it
//! ([`super::sweep::process_intents`]). A row that is later deleted (any path) queues its artifact through the 0223 trigger. A file the
//! scanner convicts, a scanner that cannot run and any other storage failure refuse the upload (`malware`,
//! `scan_unavailable`, `storage_unavailable`) and store nothing; a tenant without a readable licence cannot store a
//! file (`storage_unavailable`).
use std::future::Future;

use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use sqlx::{Acquire, PgConnection, Row};
use uuid::Uuid;

use super::artifacts::{ArtifactsClient, Fail};
use super::validate::{Accepted, reason};
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

/// The default per-tenant quota (live attachment bytes): 1 GiB. The setting that overrides it is read by the caller.
pub const DEFAULT_QUOTA_BYTES: i64 = 1024 * 1024 * 1024;

/// Who sent the file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Staff,
    Customer,
}

impl Direction {
    pub fn as_str(self) -> &'static str {
        match self {
            Direction::Staff => "staff",
            Direction::Customer => "customer",
        }
    }
}

/// A file to store: the validated bytes and the conversation they belong to.
#[derive(Clone, Copy, Debug)]
pub struct NewAttachment<'a> {
    pub conversation_id: &'a str,
    pub direction: Direction,
    /// The staff member's sub, or `customer` (the CHECK demands it for a customer upload).
    pub created_by: &'a str,
    pub accepted: &'a Accepted,
}

/// One stored attachment without its bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttachmentMeta {
    pub id: Uuid,
    /// The artifact_service artifact holding the bytes. Never shown to a client.
    pub artifact_id: Uuid,
    pub conversation_id: String,
    /// `None` until linked to the message it was sent with.
    pub message_id: Option<Uuid>,
    pub direction: Direction,
    pub media_type: String,
    pub byte_size: i32,
    pub display_name: String,
    pub width: Option<i32>,
    pub height: Option<i32>,
    pub created_by: String,
    pub created_at: DateTime<Utc>,
}

/// An attachment with its bytes (what a download or a channel delivery needs).
#[derive(Clone, Debug)]
pub struct StoredAttachment {
    pub meta: AttachmentMeta,
    pub bytes: Vec<u8>,
}

/// Where attachment bytes live. Callers hold this trait, so a change of storage changes one type.
pub trait AttachmentStore: Send + Sync {
    /// Store `new` unlinked. `attachment_quota` (413) when the tenant's live bytes plus this file exceed the quota.
    fn put(
        &self,
        conn: &mut PgConnection,
        new: NewAttachment<'_>,
    ) -> impl Future<Output = ApiResult<AttachmentMeta>> + Send;

    /// The attachment `id` of `conversation_id`, bytes included. `None` for a missing, soft-deleted or foreign id.
    fn get(
        &self,
        conn: &mut PgConnection,
        conversation_id: &str,
        id: Uuid,
    ) -> impl Future<Output = ApiResult<Option<StoredAttachment>>> + Send;

    /// Link the unlinked, live attachments `ids` of `conversation_id` to `message_id` (a message of that
    /// conversation). All or nothing: any id that is unknown, foreign, deleted or already linked, or a message of
    /// another conversation, is `ApiError::Missing`, and the caller's transaction rolls back with it.
    fn link_to_message(
        &self,
        conn: &mut PgConnection,
        conversation_id: &str,
        message_id: Uuid,
        ids: &[Uuid],
    ) -> impl Future<Output = ApiResult<()>> + Send;

    /// Remove an unsent upload (soft delete). `false` when there is no such unlinked live attachment — a sent one is
    /// part of the transcript and stays.
    fn delete(
        &self,
        conn: &mut PgConnection,
        conversation_id: &str,
        id: Uuid,
    ) -> impl Future<Output = ApiResult<bool>> + Send;
}

/// [`AttachmentStore`] over `conversation_attachment` and artifact_service.
#[derive(Clone, Debug)]
pub struct PgAttachmentStore {
    quota_bytes: i64,
    artifacts: ArtifactsClient,
}

impl PgAttachmentStore {
    /// A store whose per-tenant quota is `quota_bytes` (live bytes), keeping bytes through `artifacts`.
    pub fn new(quota_bytes: i64, artifacts: ArtifactsClient) -> Self {
        Self {
            quota_bytes,
            artifacts,
        }
    }

    /// The store of this process: the configured quota and artifact_service.
    pub fn for_state(st: &AppState) -> Self {
        Self::new(
            st.cfg.attachment_quota_bytes,
            ArtifactsClient::for_state(st),
        )
    }

    /// An upload whose request failed after [`put`](AttachmentStore::put) returned (the audit row or the commit): the
    /// metadata row is gone with the rolled-back transaction, so delete the artifact now or queue its tombstone.
    pub async fn abandon(&self, tenant: Uuid, artifact_id: Uuid) {
        self.artifacts.abandon(tenant, artifact_id).await;
    }
}

/// The error of a [`Fail`]: 422 for a convicted file, 503 for everything the caller may retry.
fn failure(f: Fail) -> ApiError {
    let (status, msg) = match f {
        Fail::Malware => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "The file was refused by the virus scan and cannot be attached.",
        ),
        Fail::ScanUnavailable => (
            StatusCode::SERVICE_UNAVAILABLE,
            "The virus scan is unavailable; try again shortly.",
        ),
        Fail::Storage => (
            StatusCode::SERVICE_UNAVAILABLE,
            "The file store is unavailable; try again shortly.",
        ),
    };
    ApiError::detailed(status, "attachment_refused", msg).with("reason", f.reason())
}

/// An upload refused for `f`, counted by its reason.
fn refused(f: Fail) -> ApiError {
    metrics::counter!("agent_attachments_refused_total", "reason" => f.reason()).increment(1);
    failure(f)
}

/// A download that failed for `f`, counted by its reason.
fn fetch_failed(f: Fail) -> ApiError {
    metrics::counter!("agent_attachment_fetch_failed_total", "reason" => f.reason()).increment(1);
    failure(f)
}

fn not_found() -> ApiError {
    ApiError::Missing("not_found", "not found".into())
}

fn meta_of(row: &sqlx::postgres::PgRow) -> Result<AttachmentMeta, sqlx::Error> {
    let direction: String = row.try_get("direction")?;
    Ok(AttachmentMeta {
        id: row.try_get("id")?,
        artifact_id: row.try_get("artifact_id")?,
        conversation_id: row.try_get("conversation_id")?,
        message_id: row.try_get("message_id")?,
        // the CHECK allows exactly these two
        direction: if direction == "customer" {
            Direction::Customer
        } else {
            Direction::Staff
        },
        media_type: row.try_get("media_type")?,
        byte_size: row.try_get("byte_size")?,
        display_name: row.try_get("display_name")?,
        width: row.try_get("width")?,
        height: row.try_get("height")?,
        created_by: row.try_get("created_by")?,
        created_at: row.try_get("created_at")?,
    })
}

/// [`AttachmentStore::link_to_message`] without a store: linking touches only the metadata rows, never the bytes, so a
/// caller that holds no `AppState` (the inbound message's transaction) can link.
pub async fn link_to_message(
    conn: &mut PgConnection,
    conversation_id: &str,
    message_id: Uuid,
    ids: &[Uuid],
) -> ApiResult<()> {
    let mut unique: Vec<Uuid> = ids.to_vec();
    unique.sort_unstable();
    unique.dedup();
    if unique.is_empty() {
        return Ok(());
    }
    let linked = sqlx::query(
        "UPDATE conversation_attachment a SET message_id = $1 \
         WHERE a.id = ANY($2) AND a.conversation_id = $3 AND a.tenant_id = app_current_tenant() \
           AND a.message_id IS NULL AND a.deleted_at IS NULL \
           AND EXISTS (SELECT 1 FROM conversation_message m \
                        WHERE m.message_id = $1 AND m.conversation_id = $3 \
                          AND m.tenant_id = a.tenant_id AND m.job_id = a.job_id)",
    )
    .bind(message_id)
    .bind(&unique)
    .bind(conversation_id)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if linked != unique.len() as u64 {
        return Err(not_found());
    }
    Ok(())
}

/// The live attachment bytes the scoped tenant holds (soft-deleted rows excluded).
pub async fn used_bytes(conn: &mut PgConnection) -> ApiResult<i64> {
    Ok(sqlx::query_scalar(
        "SELECT COALESCE(sum(byte_size), 0)::bigint FROM conversation_attachment \
         WHERE tenant_id = app_current_tenant() AND deleted_at IS NULL",
    )
    .fetch_one(conn)
    .await?)
}

/// The scoped tenant's quota: `tenants.attachment_quota_bytes` (0224), else `default`.
pub async fn tenant_quota(conn: &mut PgConnection, default: i64) -> ApiResult<i64> {
    let own: Option<i64> = sqlx::query_scalar(
        "SELECT attachment_quota_bytes FROM tenants WHERE id = app_current_tenant()",
    )
    .fetch_optional(conn)
    .await?
    .flatten();
    Ok(own.unwrap_or(default))
}

impl AttachmentStore for PgAttachmentStore {
    async fn put(
        &self,
        conn: &mut PgConnection,
        new: NewAttachment<'_>,
    ) -> ApiResult<AttachmentMeta> {
        let a = new.accepted;
        let size = i32::try_from(a.bytes.len()).map_err(|_| {
            ApiError::TooLarge("attachment_too_large", "the attachment is too large".into())
        })?;
        // serialise this tenant's uploads for the rest of the transaction, then read the live total
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('attachment_quota:' || app_current_tenant()::text, 0))")
            .execute(&mut *conn)
            .await?;
        let used: i64 = sqlx::query_scalar(
            "SELECT COALESCE(sum(byte_size), 0)::bigint FROM conversation_attachment \
             WHERE tenant_id = app_current_tenant() AND deleted_at IS NULL",
        )
        .fetch_one(&mut *conn)
        .await?;
        // the tenant's own quota (0224, set by a platform super_admin) over the platform default
        let quota = tenant_quota(&mut *conn, self.quota_bytes).await?;
        if used.saturating_add(i64::from(size)) > quota {
            metrics::counter!("agent_attachments_refused_total", "reason" => reason::QUOTA)
                .increment(1);
            return Err(ApiError::TooLarge(
                "attachment_quota",
                "the attachment storage of this workspace is full".into(),
            ));
        }
        // the tenant's own licence is the credential: none (or one that does not open) cannot store a file
        let Some(lic) = self.artifacts.licence(&mut *conn).await? else {
            return Err(refused(Fail::Storage));
        };
        let id = Uuid::new_v4();
        // commit the intent first (own transaction, 0223): a process that dies after the upload and before the row
        // leaves it behind, and the sweep finds the artifact by its `att:<id>` tag
        self.artifacts.open_intent(lic.tenant, id).await?;
        let artifact = match self.artifacts.upload(&lic, id, a).await {
            Ok(artifact) => artifact,
            Err(f) => {
                // a conviction or a scanner outage stored nothing; any other failure may have (a lost answer), so
                // the intent stays and the sweep looks
                if f != Fail::Storage {
                    self.artifacts.discard_intent(lic.tenant, id).await;
                }
                return Err(refused(f));
            }
        };
        let digest = Sha256::digest(&a.bytes);
        // the insert runs in a savepoint: a failure leaves the caller's transaction usable
        let mut sp = (&mut *conn).begin().await?;
        let inserted = sqlx::query(
            "INSERT INTO conversation_attachment \
                (id, conversation_id, direction, media_type, byte_size, sha256, display_name, artifact_id, width, \
                 height, created_by) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) \
             RETURNING id, artifact_id, conversation_id, message_id, direction, media_type, byte_size, display_name, \
                       width, height, created_by, created_at",
        )
        .bind(id)
        .bind(new.conversation_id)
        .bind(new.direction.as_str())
        .bind(a.media_type)
        .bind(size)
        .bind(digest.as_slice())
        .bind(&a.display_name)
        .bind(artifact)
        .bind(a.width.and_then(|w| i32::try_from(w).ok()))
        .bind(a.height.and_then(|h| i32::try_from(h).ok()))
        .bind(new.created_by)
        .fetch_one(&mut *sp)
        .await;
        match inserted {
            Ok(row) => {
                // the intent goes in the same transaction as the row: both commit, or neither does (then the intent
                // stays for the sweep). false = the sweep claimed it meanwhile and may delete the bytes: keep no row.
                let closed: Result<bool, sqlx::Error> =
                    sqlx::query_scalar("SELECT agent_attachment_intent_close($1, $2)")
                        .bind(lic.tenant)
                        .bind(id)
                        .fetch_one(&mut *sp)
                        .await;
                match closed {
                    Ok(true) => {
                        if let Err(e) = sp.commit().await {
                            self.artifacts
                                .abandon_upload(lic.tenant, id, artifact)
                                .await;
                            return Err(e.into());
                        }
                        Ok(meta_of(&row)?)
                    }
                    Ok(false) => {
                        let _ = sp.rollback().await;
                        self.artifacts
                            .abandon_upload(lic.tenant, id, artifact)
                            .await;
                        Err(refused(Fail::Storage))
                    }
                    Err(e) => {
                        let _ = sp.rollback().await;
                        self.artifacts
                            .abandon_upload(lic.tenant, id, artifact)
                            .await;
                        Err(e.into())
                    }
                }
            }
            Err(e) => {
                let _ = sp.rollback().await;
                // the bytes are stored and no row names them: delete them now, else queue the tombstone
                self.artifacts
                    .abandon_upload(lic.tenant, id, artifact)
                    .await;
                match e {
                    // an unknown (or other job's) conversation violates the FK: the uniform 404
                    sqlx::Error::Database(d)
                        if d.constraint() == Some("conversation_attachment_conversation") =>
                    {
                        Err(not_found())
                    }
                    e => Err(e.into()),
                }
            }
        }
    }

    async fn get(
        &self,
        conn: &mut PgConnection,
        conversation_id: &str,
        id: Uuid,
    ) -> ApiResult<Option<StoredAttachment>> {
        let row = sqlx::query(
            "SELECT id, artifact_id, conversation_id, message_id, direction, media_type, byte_size, display_name, \
                    width, height, created_by, created_at, sha256 \
             FROM conversation_attachment \
             WHERE id = $1 AND conversation_id = $2 AND tenant_id = app_current_tenant() AND deleted_at IS NULL",
        )
        .bind(id)
        .bind(conversation_id)
        .fetch_optional(&mut *conn)
        .await?;
        let Some(row) = row else { return Ok(None) };
        let meta = meta_of(&row)?;
        let sha256: Vec<u8> = row.try_get("sha256")?;
        let Some(lic) = self.artifacts.licence(&mut *conn).await? else {
            return Err(fetch_failed(Fail::Storage));
        };
        let bytes = match self.artifacts.download(&lic, meta.artifact_id).await {
            Ok(Some(b)) => b,
            Ok(None) => {
                // a live row whose artifact is gone: not "no such attachment" for the person, an outage for us
                tracing::error!(attachment = %meta.id, "attachment_bytes_missing");
                return Err(fetch_failed(Fail::Storage));
            }
            Err(f) => return Err(fetch_failed(f)),
        };
        if Sha256::digest(&bytes).as_slice() != sha256.as_slice() {
            tracing::error!(attachment = %meta.id, "attachment_bytes_changed");
            return Err(fetch_failed(Fail::Storage));
        }
        Ok(Some(StoredAttachment { meta, bytes }))
    }

    async fn link_to_message(
        &self,
        conn: &mut PgConnection,
        conversation_id: &str,
        message_id: Uuid,
        ids: &[Uuid],
    ) -> ApiResult<()> {
        link_to_message(conn, conversation_id, message_id, ids).await
    }

    async fn delete(
        &self,
        conn: &mut PgConnection,
        conversation_id: &str,
        id: Uuid,
    ) -> ApiResult<bool> {
        let done = sqlx::query(
            "UPDATE conversation_attachment SET deleted_at = now() \
             WHERE id = $1 AND conversation_id = $2 AND tenant_id = app_current_tenant() \
               AND message_id IS NULL AND deleted_at IS NULL",
        )
        .bind(id)
        .bind(conversation_id)
        .execute(&mut *conn)
        .await?
        .rows_affected();
        Ok(done == 1)
    }
}

/// The newest live IMAGE the CUSTOMER sent in `conversation_id` (never staff's, never a PDF, never another
/// conversation's), or `None`. The runtime's read (`agent_runtime` holds SELECT only): it learns that the customer
/// sent a picture, never the bytes. RLS confines it to the tenant and job.
pub async fn newest_customer_image(
    conn: &mut PgConnection,
    conversation_id: &str,
) -> ApiResult<Option<Uuid>> {
    Ok(sqlx::query_scalar(
        "SELECT id FROM conversation_attachment \
         WHERE conversation_id = $1 AND tenant_id = app_current_tenant() AND direction = 'customer' \
           AND media_type LIKE 'image/%' AND deleted_at IS NULL \
         ORDER BY created_at DESC, id DESC LIMIT 1",
    )
    .bind(conversation_id)
    .fetch_optional(&mut *conn)
    .await?)
}

/// Is `id` a live image the CUSTOMER sent in `conversation_id`? (The ownership check of a `photo_ref` input.)
pub async fn is_customer_image(
    conn: &mut PgConnection,
    conversation_id: &str,
    id: Uuid,
) -> ApiResult<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM conversation_attachment \
         WHERE id = $1 AND conversation_id = $2 AND tenant_id = app_current_tenant() AND direction = 'customer' \
           AND media_type LIKE 'image/%' AND deleted_at IS NULL)",
    )
    .bind(id)
    .bind(conversation_id)
    .fetch_one(&mut *conn)
    .await?)
}
