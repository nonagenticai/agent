//! agent-api → agent-edge (B12 §3): `POST /internal/deliver` (a staff reply, a handover notice or a CSAT request to
//! the customer's channel) and `POST /internal/notify-workers` (a new handover item to the job's worker channels).
//! Both carry `Authorization: Bearer $EDGE_INTERNAL_KEY`. On only when `EDGE_INTERNAL_KEY` and `EDGE_DELIVER_URL` are
//! both set; otherwise replies are stored and not delivered (phase A behaviour).
use uuid::Uuid;

pub use crate::channels::core::{
    DeliverKind, DeliverMessage, DeliverReq, DeliverResp, ExcerptLine, NotifyResult,
    NotifyWorkersReq, NotifyWorkersResp, WorkerItem, WorkerNoteReq, WorkerTarget,
};
use crate::config::EdgeConfig;
use crate::interaction::ConversationRef;

/// Why a delivery failed: `channel_not_configured` (the edge has no connector for the ref) or `delivery_failed`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeliveryError {
    pub code: &'static str,
}

impl DeliveryError {
    fn failed() -> Self {
        Self {
            code: "delivery_failed",
        }
    }
}

/// How a note's private comment failed (SW-11, `EdgeLink::worker_note`): `Unknown` — sent, but no answer was read (the
/// next attempt looks it up before posting); `Retry` — not posted, try again later; `Rejected` — refused by the ticket
/// or the edge's configuration, never retried.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoteError {
    Unknown,
    Retry(&'static str),
    Rejected(&'static str),
}

/// Bound on one `/internal/worker-note` call: the edge's lookup and post, each within its own 15 s call timeout.
pub const NOTE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(40);

/// The client. `Debug` shows no key.
pub struct EdgeLink {
    http: reqwest::Client,
    base: String,
    key: String,
}

impl std::fmt::Debug for EdgeLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EdgeLink")
            .field("base", &self.base)
            .finish_non_exhaustive()
    }
}

impl EdgeLink {
    /// On when both the key and the deliver URL are set.
    pub fn from_config(cfg: &EdgeConfig, http: reqwest::Client) -> Option<Self> {
        Some(Self {
            http,
            base: cfg.deliver_url.clone()?,
            key: cfg.internal_key.clone()?,
        })
    }

    /// Deliver `text` to the customer conversation `conversation`; the provider's message id on success.
    /// `last_customer_at_ms` is the customer's newest message.
    /// `name` is the staff author's display name on a staff reply (web chat shows it); `None` otherwise.
    pub async fn deliver(
        &self,
        conversation: &ConversationRef,
        kind: DeliverKind,
        text: &str,
        name: Option<&str>,
        binding_id: Option<Uuid>,
        last_customer_at_ms: Option<i64>,
    ) -> Result<String, DeliveryError> {
        self.deliver_with(
            conversation,
            kind,
            text,
            name,
            &[],
            binding_id,
            last_customer_at_ms,
        )
        .await
    }

    /// [`deliver`](Self::deliver) with the message's attachments (SW-9): refs only, never bytes (the edge fetches
    /// them from core's internal route when its channel needs them).
    #[allow(clippy::too_many_arguments)]
    pub async fn deliver_with(
        &self,
        conversation: &ConversationRef,
        kind: DeliverKind,
        text: &str,
        name: Option<&str>,
        attachments: &[crate::attachments::AttachmentRef],
        binding_id: Option<Uuid>,
        last_customer_at_ms: Option<i64>,
    ) -> Result<String, DeliveryError> {
        let body = DeliverReq {
            conversation: conversation.clone(),
            message: DeliverMessage {
                kind,
                text: text.to_string(),
                eta_minutes: None,
                name: name.map(str::to_string),
                attachments: attachments.to_vec(),
            },
            binding_id,
            last_customer_at_ms,
        };
        let resp = self
            .http
            .post(format!("{}/internal/deliver", self.base))
            .bearer_auth(&self.key)
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                tracing::warn!(error = %e.without_url(), "edge_deliver_unreachable");
                DeliveryError::failed()
            })?;
        let status = resp.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(DeliveryError {
                code: "channel_not_configured",
            });
        }
        if !status.is_success() {
            tracing::warn!(status = status.as_u16(), "edge_deliver_refused");
            return Err(DeliveryError::failed());
        }
        let a: DeliverResp = resp.json().await.map_err(|_| DeliveryError::failed())?;
        Ok(a.external_id.unwrap_or_default())
    }

    /// SW-11: post an internal staff note to the helpdesk ticket `thread` (worker side, binding `binding_id`) as a
    /// private comment. Never the customer's channel: `/internal/worker-note`, not `/internal/deliver`. `check_first`:
    /// an earlier attempt's outcome is unknown, so the edge posts only when the ticket does not hold the note yet.
    /// Bounded by [`NOTE_TIMEOUT`] (a lookup and a post, each within the edge's own call timeout).
    pub async fn worker_note(
        &self,
        tenant: Uuid,
        job_id: Uuid,
        thread: &ConversationRef,
        binding_id: Uuid,
        text: &str,
        check_first: bool,
    ) -> Result<(), NoteError> {
        let body = WorkerNoteReq {
            tenant,
            job_id,
            thread: thread.clone(),
            binding_id,
            text: text.to_string(),
            check_first,
        };
        let resp = self
            .http
            .post(format!("{}/internal/worker-note", self.base))
            .bearer_auth(&self.key)
            .timeout(NOTE_TIMEOUT)
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                // a request that timed out may have reached the ticket; one that never connected did not
                let timed_out = e.is_timeout();
                tracing::warn!(error = %e.without_url(), "edge_worker_note_unreachable");
                if timed_out {
                    NoteError::Unknown
                } else {
                    NoteError::Retry("edge_unreachable")
                }
            })?;
        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }
        tracing::warn!(status = status.as_u16(), "edge_worker_note_refused");
        Err(match status.as_u16() {
            504 => NoteError::Unknown,
            404 => NoteError::Rejected("channel_not_configured"),
            422 => NoteError::Rejected("delivery_rejected"),
            401 | 403 => NoteError::Retry("edge_unauthorized"),
            _ => NoteError::Retry("delivery_failed"),
        })
    }

    /// Tell the job's worker channels about a new item; each answer names the thread it opened.
    pub async fn notify_workers(
        &self,
        body: &NotifyWorkersReq,
    ) -> Result<Vec<NotifyResult>, DeliveryError> {
        let resp = self
            .http
            .post(format!("{}/internal/notify-workers", self.base))
            .bearer_auth(&self.key)
            .json(body)
            .send()
            .await
            .map_err(|e| {
                tracing::warn!(error = %e.without_url(), "edge_notify_unreachable");
                DeliveryError::failed()
            })?;
        if !resp.status().is_success() {
            tracing::warn!(status = resp.status().as_u16(), "edge_notify_refused");
            return Err(DeliveryError::failed());
        }
        let a: NotifyWorkersResp = resp.json().await.map_err(|_| DeliveryError::failed())?;
        Ok(a.results)
    }
}
