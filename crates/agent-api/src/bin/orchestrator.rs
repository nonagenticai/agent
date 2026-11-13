#![forbid(unsafe_code)]
//! `orchestrator` (B4 T7): one orchestrator per registered scope (tenant, job, env), each planning with its own
//! scope's design (Live in prod, Draft in test). Heartbeat + boot record for the fleet view, the design cache
//! follows the config nudges, and the offboard sweep cancels the open cases of removed scopes.
//!
//! Environment: [`RuntimeConfig`] (`DATABASE_URL` = the `agent_runtime` login, B4 0065; `REDIS_URL`; `AGENT_PREFIX`;
//! `LOG_FORMAT`) + `CASE_DEADLINE_MS` (default 3600000). No identity, licence or Keycloak variable (B4 F1). The image
//! smoke (`scripts/image-smoke.sh`) proves each one fails the boot by name.
use std::sync::Arc;

use agent_api::config::RuntimeConfig;
use agent_api::design;
use agent_api::job_design::role_switch::JobRoleSwitch;
use agent_api::job_design::toolset::DesignToolsets;
use agent_api::observability::{self, ObsConfig};
use agent_api::release::compile::LayerCompiler;
use agent_api::runtime::assistant::Assistant;
use agent_api::runtime::design::DesignCache;
use agent_api::runtime::heartbeat::{self, Serving};
use agent_api::runtime::orchestrator::{
    DesignProcedures, Orchestrator, ROLE, case_deadline_from, offboard_loop,
};
use agent_api::runtime::supervisor::run_per_scope;
use agent_api::runtime::{self, Bus};
use anyhow::{Context, Result};
use sqlx::postgres::PgPoolOptions;

fn var(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
}

#[tokio::main]
async fn main() -> Result<()> {
    // fail loud, by name, never echoing a URL
    let rt = RuntimeConfig::from_env()?;
    observability::init(&ObsConfig {
        log_format: rt.log_format,
        ..ObsConfig::default()
    })?;
    let deadline = case_deadline_from(var("CASE_DEADLINE_MS").as_deref())?;

    let root = Bus::connect(&rt.redis.url, &rt.redis.prefix).await?;
    let db = PgPoolOptions::new()
        .max_connections(5)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(&rt.database_url)
        .await
        .context("connect DATABASE_URL (agent_runtime)")?;
    let compiler = Arc::new(LayerCompiler::new());
    let reg = Arc::new(design::registry(compiler));
    // B5 T12: the assistant role (B5's switch and the one toolset source) when the env allows it; never a new boot
    // requirement (none = no assistant: a request without procedure hands over, as before)
    let assistant = agent_api::state::optional_runtime(db.clone()).map(|st| {
        Arc::new(
            Assistant::new(st)
                .with_switch(Arc::new(JobRoleSwitch))
                .with_toolset(Arc::new(DesignToolsets)),
        )
    });
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
    let _offboard = tokio::spawn(offboard_loop(root.clone(), db.clone()));
    tracing::info!(%consumer, deadline_ms = deadline.as_millis() as u64, "orchestrator starting");

    run_per_scope(root, ROLE, move |bus, _scope| {
        let built = Orchestrator::new(
            bus,
            db.clone(),
            reg.clone(),
            designs.clone(),
            DesignProcedures,
        )
        .map(|o| {
            let o = o
                .with_case_deadline(deadline)
                .with_consumer(consumer.clone())
                .with_serving(serving.clone());
            match &assistant {
                Some(a) => o.with_assistant(a.clone()),
                None => o,
            }
        });
        async move { Arc::new(built?).run().await }
    })
    .await
}
