//! Conversation attachments (Staff Workspace SW-9): a photo or PDF a customer sends or a staff member attaches.
//!
//! - [`validate`]: THE gate for untrusted bytes. Magic-byte allowlist (JPEG, PNG, WebP, PDF), size and decompression-
//!   bomb limits checked from the header BEFORE any decode, animated input refused, images re-encoded (which strips
//!   EXIF/GPS and kills polyglots), PDFs parsed under hard limits and refused when they carry active content.
//!   Also the customer-visible display name (no path, no control characters, never an id).
//! - [`store`]: the [`store::AttachmentStore`] trait (put, get, link, delete) and its implementation over
//!   `conversation_attachment` (0205 + 0223, metadata, RLS-scoped, per-tenant quota) and the platform artifact_service
//!   ([`artifacts`]: the bytes, scanned there by ClamAV).
//! - [`sweep`]: the 1 h sweep of unsent uploads, the tombstone worker that deletes the bytes of every deleted row and
//!   the resolver of upload intents (0223: an upload committed before its bytes leave, so a crash between upload and
//!   row leaves nothing behind).
//!
//! Only [`validate::Accepted`] values reach the store. Bytes, names and hashes are data: never in a log line, an audit
//! detail or a metric label (counters carry the media type or the refusal reason, both from closed sets).
pub mod artifacts;
pub mod store;
pub mod sweep;
pub mod validate;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// What a message, a stream frame, the history or a channel delivery says about an attachment (SW-9 contract): never the
/// bytes, the hash, who uploaded it or an URL. The customer-visible name is [`validate`]'s sanitised display name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentRef {
    pub id: Uuid,
    pub media_type: String,
    pub display_name: String,
    pub byte_size: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<i32>,
}

impl From<&store::AttachmentMeta> for AttachmentRef {
    fn from(m: &store::AttachmentMeta) -> Self {
        Self {
            id: m.id,
            media_type: m.media_type.clone(),
            display_name: m.display_name.clone(),
            byte_size: m.byte_size,
            width: m.width,
            height: m.height,
        }
    }
}
