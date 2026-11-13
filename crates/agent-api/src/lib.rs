#![forbid(unsafe_code)]
//! agent-api: the backend of agent.nonagentic.ai (ns `agent`, D53). Identity (portal-realm OIDC, client `agent`),
//! tenancy (tenants/users under RLS, copied from care-bus-agents@45ee128), `/health`, `/ready`, `GET /api/me`,
//! the early-access signup (D73; dark unless `EARLY_ACCESS_SIGNUP_ENABLED`), and the channel-agnostic interaction API
//! (crate::interaction) with its voice connector (crate::voice). The public `agent-edge` binary (src/bin/agent-edge.rs)
//! serves the prebuilt channel connectors (crate::channels) from the same library.
pub mod attachments;
pub mod audit;
pub mod auth;
pub mod bus_admin;
pub mod cases;
pub mod channels;
pub mod config;
pub mod connectors;
pub mod conversations;
pub mod crypto;
pub mod design;
pub mod discovery;
pub mod error;
pub mod generator;
pub mod handover;
pub mod iam;
pub mod identity;
pub mod interaction;
pub mod job_design;
pub mod journey;
pub mod keycloak_admin;
pub mod knowledge;
pub mod lease;
pub mod licenses_admin;
pub mod licensing;
pub mod mcp_client;
pub mod members;
pub mod netguard;
pub mod observability;
pub mod page;
pub mod party;
pub mod platform;
pub mod platform_db;
pub mod providers;
pub mod provision;
pub mod readiness;
pub mod readmodel;
pub mod release;
pub mod routes;
pub mod runtime;
pub mod service_token;
pub mod signup;
pub mod sse_hub;
pub mod state;
pub mod strata;
pub mod support;
pub mod tabular;
pub mod tenancy;
pub mod tenants;
pub mod turn;
pub mod turnstile;
pub mod upstream;
pub mod usage;
pub mod validation;
pub mod voice;

pub use routes::router;
