//! The in-process event hub behind the SSE feeds (`GET /api/jobs/{job}/events`, `…/conversations/{id}/events`). One
//! tokio broadcast channel per process (capacity 1024); a subscriber sees only the frames of its tenant and job (and
//! conversation, for a conversation feed). A slow subscriber of [`feed`] skips the frames it missed (the served
//! feeds close it instead). Frames are published after the write that produced them commits. B4 keeps [`LocalHub`]
//! and bridges Redis Pub/Sub into it (crate::sse_hub: the runtime processes and other replicas); the feeds
//! themselves are served there.
use std::convert::Infallible;
use std::time::Duration;

use axum::response::sse::{Event, KeepAlive, Sse};
use futures_util::Stream;
use serde_json::Value;
use tokio::sync::broadcast;
use uuid::Uuid;

/// Frames a broadcast channel keeps for slow subscribers.
pub const CAPACITY: usize = 1024;

/// One published change.
#[derive(Clone, Debug)]
pub struct Frame {
    pub tenant: Uuid,
    pub job_id: Uuid,
    /// The conversation it concerns (every frame of this module has one).
    pub conversation_id: Option<String>,
    /// The SSE event name: `message`, `case`, `handover`, `owner`, `csat`, `delivery_failed`.
    pub event: &'static str,
    pub data: Value,
}

pub trait EventHub: Send + Sync {
    fn publish(&self, frame: Frame);
    fn subscribe(&self) -> broadcast::Receiver<Frame>;
}

/// The single-process hub.
pub struct LocalHub {
    tx: broadcast::Sender<Frame>,
}

impl Default for LocalHub {
    fn default() -> Self {
        Self {
            tx: broadcast::channel(CAPACITY).0,
        }
    }
}

impl EventHub for LocalHub {
    fn publish(&self, frame: Frame) {
        // No subscriber is not an error.
        let _ = self.tx.send(frame);
    }

    fn subscribe(&self) -> broadcast::Receiver<Frame> {
        self.tx.subscribe()
    }
}

/// Does `f` belong to the feed of (`tenant`, `job_id`, `conversation`)?
pub fn matches(f: &Frame, tenant: Uuid, job_id: Uuid, conversation: Option<&str>) -> bool {
    f.tenant == tenant
        && f.job_id == job_id
        && conversation.is_none_or(|c| f.conversation_id.as_deref() == Some(c))
}

/// The frames of one feed as an SSE stream (keep-alive comments every 15 s).
pub fn feed(
    hub: &dyn EventHub,
    tenant: Uuid,
    job_id: Uuid,
    conversation: Option<String>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>> + use<>> {
    let rx = hub.subscribe();
    let s = futures_util::stream::unfold((rx, conversation), move |(mut rx, conv)| async move {
        loop {
            match rx.recv().await {
                Ok(f) if matches(&f, tenant, job_id, conv.as_deref()) => {
                    let ev = Event::default().event(f.event).data(f.data.to_string());
                    return Some((Ok(ev), (rx, conv)));
                }
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    });
    Sse::new(s).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(t: Uuid, j: Uuid, c: &str) -> Frame {
        Frame {
            tenant: t,
            job_id: j,
            conversation_id: Some(c.into()),
            event: "message",
            data: Value::Null,
        }
    }

    #[test]
    fn frames_are_filtered_by_tenant_job_and_conversation() {
        let (t, j) = (Uuid::new_v4(), Uuid::new_v4());
        let f = frame(t, j, "conv-1");
        assert!(matches(&f, t, j, None));
        assert!(matches(&f, t, j, Some("conv-1")));
        assert!(!matches(&f, t, j, Some("conv-2")));
        assert!(!matches(&f, Uuid::new_v4(), j, None));
        assert!(!matches(&f, t, Uuid::new_v4(), None));
    }

    #[tokio::test]
    async fn local_hub_delivers_to_subscribers() {
        let hub = LocalHub::default();
        hub.publish(frame(Uuid::nil(), Uuid::nil(), "x"));
        let mut rx = hub.subscribe();
        hub.publish(frame(Uuid::nil(), Uuid::nil(), "y"));
        assert_eq!(
            rx.recv().await.unwrap().conversation_id.as_deref(),
            Some("y")
        );
    }
}
