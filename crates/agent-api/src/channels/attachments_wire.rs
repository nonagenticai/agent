//! SW-9: the wire between the edge and core for a file a customer sends on a channel the EDGE downloads (Telegram). The
//! edge fetches the bytes from the provider, core validates and stores them (`crate::attachments::validate::accept`,
//! the same gate as every other upload), and the edge sends the stored attachment's id with the inbound message
//! (`InboundReq.attachments`), which core links to that message.
//!
//! `POST /api/internal/channel/attachments` (EdgeKey bearer), multipart: part `meta` ([`InboundUploadMeta`] as JSON) and
//! part `file` (the raw bytes). Core resolves tenant, job and the conversation from the binding and `conversation`,
//! never trusting more than the binding row confirms. Answers:
//! - 201 [`AttachmentRef`](crate::attachments::AttachmentRef) (direction `customer`, unlinked, swept after 1 h);
//! - 422 `{"error":{"code":"attachment_refused","reason":"<validate reason>"}}`: [`UploadOutcome::Refused`];
//! - 413 `{"error":{"code":"attachment_quota"|"too_large"}}`: [`UploadOutcome::Refused`] as `quota` / `too_large`;
//! - anything else: a [`CoreError`].
//!
//! Bytes, file names and the provider's file handle are data: never in a log line, an error or a metric label.
use std::time::Duration;

use reqwest::multipart::{Form, Part};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use super::connector::ConversationRef;
use super::core::{CoreError, CoreLink};
use crate::attachments::AttachmentRef;

/// The path of the upload route on agent-api's internal seam.
pub const UPLOAD_PATH: &str = "/api/internal/channel/attachments";
/// The bound on one upload call (the file is at most 10 MB).
pub const UPLOAD_TIMEOUT: Duration = Duration::from_secs(30);

/// What core needs to place an inbound file: the binding the message arrived on and where it arrived.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InboundUploadMeta {
    pub binding_id: Uuid,
    /// The channel kind (`ChannelKind::as_str`).
    pub kind: String,
    pub tenant: Uuid,
    pub job_id: Uuid,
    pub conversation: ConversationRef,
    /// The provider's message id of the inbound message the file belongs to (the `message_id` of its `inbound` call).
    pub message_id: String,
    /// The customer (`WireParty.id`).
    pub party_id: String,
    /// The provider's file name, only a hint for the display name (core sanitises it).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub declared_name: String,
}

/// What core made of an uploaded file.
#[derive(Clone, Debug, PartialEq)]
pub enum UploadOutcome {
    Stored(AttachmentRef),
    /// Refused for good; `reason` is a `validate::reason` code (or `quota`), safe to show as plain words.
    Refused {
        reason: String,
    },
}

/// Hand one downloaded file to core. `Err` = core could not be asked (the file is lost for this message).
pub async fn upload_inbound(
    core: &CoreLink,
    meta: &InboundUploadMeta,
    bytes: Vec<u8>,
) -> Result<UploadOutcome, CoreError> {
    let meta_json =
        serde_json::to_string(meta).map_err(|_| CoreError::Unavailable("encode".into()))?;
    let meta_part = Part::text(meta_json)
        .mime_str("application/json")
        .map_err(|_| CoreError::Unavailable("encode".into()))?;
    let file_part = Part::bytes(bytes)
        .file_name("upload")
        .mime_str("application/octet-stream")
        .map_err(|_| CoreError::Unavailable("encode".into()))?;
    let form = Form::new().part("meta", meta_part).part("file", file_part);
    let resp = core
        .http
        .post(format!("{}{UPLOAD_PATH}", core.base_url))
        .bearer_auth(core.key.as_str())
        .timeout(UPLOAD_TIMEOUT)
        .multipart(form)
        .send()
        .await
        .map_err(|e| {
            CoreError::Unavailable(if e.is_timeout() {
                "timeout".into()
            } else {
                "connect".into()
            })
        })?;
    let status = resp.status();
    let body = resp
        .bytes()
        .await
        .map_err(|_| CoreError::Unavailable("read".into()))?;
    outcome_of(status.as_u16(), &body)
}

/// The upload route's answer as an outcome (split out for tests).
pub fn outcome_of(status: u16, body: &[u8]) -> Result<UploadOutcome, CoreError> {
    let v: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let code = v
        .pointer("/error/code")
        .and_then(Value::as_str)
        .unwrap_or("");
    match status {
        200 | 201 => serde_json::from_value::<AttachmentRef>(v)
            .map(UploadOutcome::Stored)
            .map_err(|_| CoreError::Unavailable("invalid_body".into())),
        422 if code == "attachment_refused" => Ok(UploadOutcome::Refused {
            reason: v
                .pointer("/error/reason")
                .and_then(Value::as_str)
                .unwrap_or("unsupported_type")
                .to_string(),
        }),
        413 => Ok(UploadOutcome::Refused {
            reason: if code == "attachment_quota" {
                "quota".into()
            } else {
                "too_large".into()
            },
        }),
        s if s >= 500 => Err(CoreError::Unavailable(format!("status_{s}"))),
        _ => Err(CoreError::Code(if code.is_empty() {
            format!("status_{status}")
        } else {
            code.to_string()
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn answers_map_to_outcomes() {
        let id = Uuid::nil();
        let ok = json!({"id": id, "media_type": "image/jpeg", "display_name": "photo.jpg", "byte_size": 5});
        assert!(matches!(
            outcome_of(201, ok.to_string().as_bytes()),
            Ok(UploadOutcome::Stored(r)) if r.id == id
        ));
        let refused =
            json!({"error": {"code": "attachment_refused", "reason": "active_pdf"}}).to_string();
        assert_eq!(
            outcome_of(422, refused.as_bytes()),
            Ok(UploadOutcome::Refused {
                reason: "active_pdf".into()
            })
        );
        // the admin switched customer files off: the closed reason reaches the connector, which tells the customer
        let off = json!({"error": {"code": "attachment_refused", "reason": "attachments_off"}})
            .to_string();
        assert_eq!(
            outcome_of(422, off.as_bytes()),
            Ok(UploadOutcome::Refused {
                reason: "attachments_off".into()
            })
        );
        assert!(
            crate::channels::connector::refusal_notice("attachments_off")
                .contains("turned off for this chat")
        );
        let quota = json!({"error": {"code": "attachment_quota"}}).to_string();
        assert_eq!(
            outcome_of(413, quota.as_bytes()),
            Ok(UploadOutcome::Refused {
                reason: "quota".into()
            })
        );
        assert_eq!(
            outcome_of(413, b"{}"),
            Ok(UploadOutcome::Refused {
                reason: "too_large".into()
            })
        );
        assert!(matches!(
            outcome_of(503, b""),
            Err(CoreError::Unavailable(_))
        ));
        assert_eq!(
            outcome_of(
                404,
                json!({"error": {"code": "job_not_found"}})
                    .to_string()
                    .as_bytes()
            ),
            Err(CoreError::Code("job_not_found".into()))
        );
        assert!(outcome_of(201, b"nope").is_err());
    }
}
