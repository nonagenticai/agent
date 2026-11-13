//! Conversations, handover, the operator console, CSAT and channel bindings (B12; the B4 T10/T12/T16/T16b/T17 slice
//! and B11 phase B on the agent-api side).
//!
//! - [`store`]: conversations by channel ref, cases (`CS-n`), the message log (provider-retry dedupe on
//!   `external_id`), the last-20-turn history, console control.
//! - [`ownership`]: THE owner of a conversation (`conversation_control`: agent | queue | staff, SW-1); the only
//!   writer of the owner row; a handover item's assignee is derived from it.
//! - [`handover`]: the handover queue (D64): items (`HO-n`), events, assignment, take-next, the state machine.
//! - [`staff`]: THE one staff-message path (Send: reply or note; the worker-thread door).
//! - [`csat`]: the CSAT request after a staff resolve and its answer (a lone `1`–`5`, or `submitCsat`).
//! - [`hub`]: the in-process event hub behind the SSE feeds.
//! - [`human`]: "the customer asks for a person" (en/nl/de, conservative).
//! - [`bindings`]: channel bindings (customer and worker side), BYO credentials sealed (`binding_cred:<id>`).
//! - [`edge_link`]: agent-api → agent-edge (`/internal/deliver`, `/internal/notify-workers`).
//! - [`internal`]: agent-edge → agent-api (`/api/internal/channel/*`), key-gated.
//! - [`routes`]: the job-scoped admin routes (B4 names).
//! - [`webchat_sessions`]: web chat sessions, durable rate limits and abuse blocks (B13), key-gated.
//! - [`webchat_stream`]: the web chat customer stream (later messages), published on the job's fan-out and read by
//!   any edge replica, key-gated.
//! - [`answer_sweeper`]: answers and delivers `accept_only` messages whose edge never asked for the answer (B7 R6).
//! - [`note_mirror`]: a staff note's private comment on the helpdesk ticket, as a durable outbox (SW-11).
//!
//! Customer and staff text is data: never in a log line, an audit detail or a metric label.
pub mod answer_sweeper;
pub mod attachment_settings;
pub mod attachments_routes;
pub mod bindings;
pub mod csat;
pub mod edge_link;
pub mod handover;
pub mod hub;
pub mod human;
pub mod internal;
pub mod note_mirror;
pub mod ownership;
pub mod routes;
pub mod staff;
pub mod state;
pub mod store;
pub mod webchat_attachments;
pub mod webchat_identity;
pub mod webchat_sessions;
pub mod webchat_stream;

/// The canonical channel set (B12 §1: every `<table>_channel_check`).
pub const CHANNELS: [&str; 5] = ["staff", "telegram", "web_chat", "voice", "api"];

/// The connector kinds a binding may name (B12 §2).
/// `telegram` (B7 T15b, R6): customer side only, the tenant's own bot. `helpdesk` (B7 R9): worker side only, Frappe.
pub const BINDING_KINDS: [&str; 3] = ["web_chat", "telegram", "helpdesk"];

/// Milliseconds since the epoch, now.
pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// An RFC 3339 UTC timestamp with milliseconds (`2026-10-20T10:00:00.000Z`).
pub fn iso(t: chrono::DateTime<chrono::Utc>) -> String {
    t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// [`iso`] of an epoch-millisecond instant.
pub fn iso_ms(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(iso)
        .unwrap_or_default()
}
