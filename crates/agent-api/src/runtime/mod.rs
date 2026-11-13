//! The B4 runtime (ported from care-bus-agents@96c608c `crates/carebus`): a Redis Streams bus partitioned by scope
//! (tenant, job, env), the scope registry and the per-scope supervisor every runtime process runs, config nudges
//! and heartbeats.
//!
//! ```text
//! agent-api (drain) → bus:customer:{area} (request.qualified) → orchestrator
//! orchestrator → bus:op (op.call) → runner → bus:op:results (op.result) → orchestrator
//! orchestrator → bus:customer:{area} (request.resolved) → interaction
//! ```
//!
//! Every publish is mirrored on the scope's Pub/Sub channel `console` (ids and codes only for `op.*`, K50) and kept
//! in `console:recent`. All keys live under `{AGENT_PREFIX}s:{tenant}:{job}:` (prod) or
//! `{AGENT_PREFIX}s:{tenant}:{job}:test:` (test, R5); the registry, nudges and heartbeats are root keys.
//!
//! agent-api holds the root bus as `AppState.redis` (`None` while `REDIS_URL` is unset: the runtime is dark and
//! agent-api still boots).
pub mod assistant;
pub mod backend;
pub mod bus;
pub mod case;
pub mod catalog;
pub mod conditions;
pub mod config_nudge;
pub mod customer_context;
pub mod declared;
pub mod design;
pub mod domain;
pub mod gates;
pub mod heartbeat;
pub mod id_guard;
pub mod interaction;
pub mod orchestrator;
pub mod prompt;
pub mod reply;
pub mod reply_gate;
pub mod resolve;
pub mod routes;
pub mod runner;
pub mod scope;
pub mod scopes;
pub mod spaces;
pub mod supervisor;
pub mod task_answer;
pub mod task_relay;
pub mod trace;

pub use bus::{Bus, ConsumeOptions, Envelope, Streams};
pub use scope::{Env, Scope};

/// `{role}-{HOSTNAME}-{pid}`: a consumer name unique per process (the pod name in the cluster).
pub fn consumer_name(role: &str) -> String {
    let host = std::env::var("HOSTNAME").unwrap_or_else(|_| "local".into());
    format!("{role}-{host}-{}", std::process::id())
}
