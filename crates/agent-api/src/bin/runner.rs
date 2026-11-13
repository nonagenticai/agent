#![forbid(unsafe_code)]
//! `runner` (B4 T6): one operation runner per registered scope (tenant, job, env), consuming `op.call` and
//! publishing `op.result` (crate::runtime::runner). Heartbeat for the fleet view; the design cache follows the
//! config nudges. No HTTP listener.
//!
//! Environment: [`RunnerConfig`] (B4 F1) — the daemon variables (`DATABASE_URL` = the `agent_runtime` login, B4
//! 0065; `REDIS_URL`; `AGENT_PREFIX`; `LOG_FORMAT`) plus agent-api's connector policy, `TENANT_DATA_KEYS` for live
//! systems' credentials, the strata URLs for `knowledge.*` and the provider settings. NO identity, signup or token
//! verifier variable: the runner holds no Keycloak-admin or licences-admin credential and no licence-token key
//! (`AppState::identity` is inert; no runner path uses it); any of them still set is named in one boot warning.
//! `MIGRATE_DATABASE_URL` is REFUSED: the runner calls live systems and must never hold the owner login
//! (`state::build_runtime` opens no owner pool).
use std::sync::Arc;

use agent_api::config::RunnerConfig;
use agent_api::observability::{self, ObsConfig};
use agent_api::runtime::design::DesignCache;
use agent_api::runtime::heartbeat::{self, Serving};
use agent_api::runtime::runner::{ROLE, Runner};
use agent_api::runtime::supervisor::run_per_scope;
use agent_api::runtime::{self, Bus};
use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    // fail loud, by name, never echoing a URL
    let rc = RunnerConfig::from_env()?;
    observability::init(&ObsConfig {
        log_format: rc.runtime.log_format,
        ..ObsConfig::default()
    })?;
    if !rc.ignored.is_empty() {
        tracing::warn!(vars = ?rc.ignored, "runner_ignores_identity_env");
    }
    let answer_timeout = rc.app.mcp.as_ref().map_or(
        std::time::Duration::from_secs(agent_api::config::MCP_TASK_ANSWER_TIMEOUT_DEFAULT_SECS),
        |m| m.answer_timeout,
    );
    let st = agent_api::state::build_runtime(rc.app).await?;
    // Durable MCP tasks (crate::mcp_client::store): one poller per runner process; claims are leased, so replicas share.
    // Every task's row names its server (`origin`), so the poller runs whether or not the platform MCP is configured.
    let (_mcp_stop, mcp_stopped) = tokio::sync::watch::channel(false);
    let poller = Arc::new(agent_api::mcp_client::store::TaskPoller {
        db: st.db.clone(),
        clients: st.task_clients(),
        lease_s: 60,
        batch: 20,
        answer_timeout,
    });
    tokio::spawn(poller.work(mcp_stopped, std::time::Duration::from_secs(2)));
    tracing::info!("mcp task poller started");
    let root = Bus::connect(&rc.runtime.redis.url, &rc.runtime.redis.prefix).await?;
    let designs = Arc::new(DesignCache::default());
    let _follow = designs.follow(root.clone());
    let consumer = runtime::consumer_name(ROLE);
    let serving = Serving::default();
    let _beat = tokio::spawn(heartbeat::run(
        root.clone(),
        ROLE.to_owned(),
        consumer.clone(),
        serving.clone(),
    ));
    tracing::info!(%consumer, "runner starting");

    run_per_scope(root, ROLE, move |bus, _scope| {
        // B5 T12: the tool gate per call (toolset, then the P2 questions covering the operation)
        let built = Runner::new(bus, st.clone(), designs.clone()).map(|r| {
            r.with_consumer(consumer.clone())
                .with_gate(Arc::new(agent_api::job_design::gates::hooks::ToolPoint))
        });
        async move { Arc::new(built?).run().await }
    })
    .await
}
