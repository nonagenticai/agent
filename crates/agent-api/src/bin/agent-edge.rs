#![forbid(unsafe_code)]
//! `agent-edge`: the public channel edge (`chat.nonagentic.ai`, plan B11 phase A). Provider webhooks (Telegram,
//! Frappe Helpdesk) and the phase-A web chat, all answered through the interaction seam. No admin route, no Keycloak, no
//! database in phase A (crate::channels::config). B12: with `EDGE_CORE_URL` the core bindings refresh every 60 s
//! (the first pull retried with backoff until agent-api answers, B13).
use std::future::IntoFuture;

use agent_api::channels::config::EdgeConfig;
use agent_api::{channels, observability};
use anyhow::Result;
use tokio::sync::watch;

#[tokio::main]
async fn main() -> Result<()> {
    // Fail loud: a malformed variable stops the process here, by name.
    let cfg = EdgeConfig::from_env()?;
    let metrics = observability::init(&cfg.obs)?;
    cfg.log_status();
    let st = cfg.build()?;
    for s in st.registry.statuses() {
        tracing::info!(
            channel = s.kind.as_str(),
            bindings = s.bindings,
            "edge_channel_on"
        );
    }
    tracing::info!(web_chat = st.webchat.is_some(), "edge_web_chat");
    // B12/B13: pull the core bindings in the background, retrying 1 s, 2 s, 4 s … (cap 30 s) until agent-api answers,
    // then every 60 s (a failure keeps the last good set). Serving starts at once with the env bindings.
    if let Some(core) = st.core.clone() {
        let _refresh = st
            .bindings
            .clone()
            .spawn_refresh(core, channels::bindings::REFRESH_EVERY);
    }
    // B7 T15b: Telegram webhook health observed here, reported to core every 60 s.
    let _health = channels::webhook_health::spawn_reporter(st.clone());
    let app = channels::router(st);
    let listener = tokio::net::TcpListener::bind(&cfg.bind).await?;
    let metrics_listener = tokio::net::TcpListener::bind(&cfg.obs.metrics_bind).await?;
    tracing::info!(bind = %cfg.bind, metrics_bind = %cfg.obs.metrics_bind, "agent-edge listening");
    let (stop_tx, stop_rx) = watch::channel(false);
    tokio::spawn(async move {
        shutdown().await;
        let _ = stop_tx.send(true);
    });
    // The client address (per-IP web chat limits) comes from the connection when no proxy header names it.
    let edge = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(stopped(stop_rx.clone()));
    let prom = axum::serve(metrics_listener, observability::metrics_router(metrics))
        .with_graceful_shutdown(stopped(stop_rx));
    tokio::try_join!(edge.into_future(), prom.into_future())?;
    Ok(())
}

async fn stopped(mut rx: watch::Receiver<bool>) {
    let _ = rx.wait_for(|stop| *stop).await;
}

async fn shutdown() {
    let interrupt = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    let terminate = async {
        if let Ok(mut signal) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            signal.recv().await;
        } else {
            std::future::pending::<()>().await;
        }
    };
    tokio::select! { _ = interrupt => {}, _ = terminate => {} }
}
