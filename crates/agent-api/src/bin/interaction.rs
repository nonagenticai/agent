#![forbid(unsafe_code)]
//! `interaction`: the B4 interaction daemon (crate::runtime::interaction). Consumes `request.resolved` of every
//! registered scope and answers the customer: the reply, its grounding and the CSAT request in one transaction, then
//! the `chat` frames. B5 T12: with the optional runtime state (`state::optional_runtime`) every reply first passes
//! the R1 gate. No HTTP listener.
//!
//! Environment (fail loud, by name): [`RuntimeConfig`] only — `DATABASE_URL` (the `agent_runtime` role in the
//! cluster, under RLS), `REDIS_URL`, `AGENT_PREFIX` (optional, no whitespace), `LOG_FORMAT` (`json` default |
//! `pretty`). No identity, licence or Keycloak variable (B4 F1).
use std::sync::Arc;

use agent_api::config::RuntimeConfig;
use agent_api::design;
use agent_api::observability::{self, ObsConfig};
use agent_api::release::compile::LayerCompiler;
use agent_api::runtime::Bus;
use agent_api::runtime::interaction::{DB_CONNECT_TIMEOUT, run_daemon};
use anyhow::Result;
use sqlx::postgres::PgPoolOptions;

#[tokio::main]
async fn main() -> Result<()> {
    let rt = RuntimeConfig::from_env()?;
    observability::init(&ObsConfig {
        log_format: rt.log_format,
        ..ObsConfig::default()
    })?;
    // The errors below never carry the URLs (they may hold passwords).
    let db = agent_api::state::transaction_safe(PgPoolOptions::new())
        .max_connections(8)
        .acquire_timeout(DB_CONNECT_TIMEOUT)
        .connect(&rt.database_url)
        .await
        .map_err(|e| anyhow::anyhow!("connect to Postgres (DATABASE_URL): {}", db_error(&e)))?;
    let root = Bus::connect(&rt.redis.url, &rt.redis.prefix).await?;
    let compiler = Arc::new(LayerCompiler::new());
    let registry = Arc::new(design::registry(compiler));
    // B5 T12: the R1 gate when the env allows it (never a new boot requirement; none = replies are not gated)
    let st = agent_api::state::optional_runtime(db.clone());
    run_daemon(db, registry, root, st).await
}

/// A connection error without the URL: the database's own code, else the error kind.
fn db_error(e: &sqlx::Error) -> String {
    match e {
        sqlx::Error::Database(d) => d.code().map(|c| c.to_string()).unwrap_or_default(),
        sqlx::Error::PoolTimedOut => "pool timed out".into(),
        sqlx::Error::Io(_) => "io error".into(),
        sqlx::Error::Tls(_) => "tls error".into(),
        _ => "connection failed".into(),
    }
}
