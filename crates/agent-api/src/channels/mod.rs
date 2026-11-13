//! Channels: prebuilt connectors that bring a tenant's customers (and, phase B, its service workers) to a job's agent
//! from the tools they already use (D103, plan B11), all through the interaction seam (D101). They run on the public
//! `agent-edge` binary (`chat.nonagentic.ai`), never on `agent-api`.
//!
//! - [`connector`]: the generic [`ChannelConnector`] interface (B9 T1; `side` + `deliver` per D104).
//! - [`edge`]: the edge router and pipeline (`authenticate → ingest → ack`, then `seam → deliver`).
//! - [`config`]: the edge's env (no database, no Keycloak in phase A).
//! - [`bindings`]: provider workspace → job bindings: env (phase A) ∪ core (`0120_connector_bindings`, B12).
//! - [`core`]: the internal seam to agent-api (B12): client + wire types.
//! - [`memory`]: bounded in-process conversation turns and provider-retry dedupe.
//! - [`telegram`]: customer-side, BYO bot per core binding only (B7 T15b, R6).
//! - [`telegram_html`]: the agent's markdown as Telegram HTML parts (`parse_mode: "HTML"`).
//! - [`workers`]: shared pieces of the worker-side connectors (handover card text, fixed confirmations).
//! - [`helpdesk`]: Frappe Helpdesk, worker side only (B7 R9: HD Ticket out, signed Frappe Webhook in).
//! - [`webchat`]: the phase-A web chat (stateless, the widget holds the history; B9 adds sessions and Party).
//! - [`webhook_health`]: Telegram webhook health as the edge observes it, reported to core every 60 s (B7 T15b).
pub mod attachments_wire;
pub mod bindings;
pub mod config;
pub mod connector;
pub mod core;
pub mod edge;
pub mod helpdesk;
pub mod memory;
pub mod telegram;
pub mod telegram_html;
pub mod webchat;
pub mod webhook_health;
pub mod workers;

pub use connector::{
    ChannelConnector, ChannelError, ChannelKind, ChannelStatus, ConversationRef, DeliveryReceipt,
    EdgeReply, ExternalIdentity, Inbound, Ingress, IngressAuth, IngressRequest, Outbound, Side,
    StaffInbound,
};
pub use edge::{EdgeState, Registry, router};
