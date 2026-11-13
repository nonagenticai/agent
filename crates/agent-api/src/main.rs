#![forbid(unsafe_code)]
use std::future::IntoFuture;

use agent_api::config::Config;
use agent_api::signup::renotify::Renotice;
use agent_api::{observability, router, state};
use anyhow::Result;
use tokio::sync::watch;

#[tokio::main]
async fn main() -> Result<()> {
    // Fail loud: a missing or malformed variable stops the process here, by name.
    let cfg = Config::from_env()?;
    let metrics = observability::init(&cfg.obs)?;
    // `agent-api renotify-signup <signupId> [--apply]`: re-post one pending signup's Telegram notice and exit.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("renotify-signup") {
        std::process::exit(renotify(cfg, &args[1..]).await);
    }
    let bind = cfg.bind.clone();
    let metrics_bind = cfg.obs.metrics_bind.clone();
    let st = state::build(cfg).await?;
    let _refresh = observability::spawn_refresh(st.db.clone(), metrics.clone());
    let _sweep = agent_api::members::seen::spawn_sweep(st.db.clone());
    let _drainer = agent_api::provision::spawn_drainer(st.clone());
    // P4 T8: hourly drift reconcile (read-only against strata, startup jitter <= 10 min); agent-api only, never edge
    let _reconciler = agent_api::provision::spawn_reconciler(st.clone());
    // B4 T11: the metering outbox → billing; spawns only with METERING_EMIT_ENABLED=true (K52: off).
    let _metering = agent_api::usage::drainer::spawn(st.clone());
    // B4 T4: design writers' config nudges (queued as Postgres NOTIFY, sent on commit) → Redis, when REDIS_URL is set
    let _nudges = st
        .redis
        .clone()
        .map(|r| agent_api::runtime::design::spawn_nudge_relay(st.db.clone(), r));
    // B4: keep the scope registry rebuilt from the database (retried until Redis accepts, then every 5 min), when
    // REDIS_URL is set
    let _scopes = st.redis.clone().map(|r| {
        agent_api::runtime::scopes::spawn_reconciler(
            st.db.clone(),
            r,
            agent_api::runtime::scopes::RECONCILE_EVERY,
        )
    });
    // B4 T9: re-drain conversation inboxes whose turn died (every 5 s per scope), when REDIS_URL is set
    let _inboxes = agent_api::turn::sweeper::spawn(st.clone());
    // B2: MCP task questions and results reach their conversation
    let _task_relay = agent_api::runtime::task_relay::spawn(st.clone());
    // B13: durable async interactions (claims queued callbacks, also those left by a previous process)
    let _deliveries = agent_api::interaction::webhook::spawn_worker(st.clone());
    // D82: spawns only when SOURCE_SCHEDULE_ENABLED is on, which boot refuses (no timer may trigger GPU calls).
    let _schedule = agent_api::knowledge::schedule::spawn(st.clone());
    // B13 §3: the 10-minute sweep of the durable web chat rate counters (windows older than 2 h)
    let _webchat_sweep = agent_api::conversations::webchat_sessions::spawn_sweep(st.db.clone());
    // SW-9: the 10-minute sweep of unsent attachment uploads (older than 1 h)
    let _attachment_sweep = agent_api::attachments::sweep::spawn_sweep(st.clone());
    // SW-9: every minute delete the artifact_service bytes of deleted attachments (tombstones)
    let _attachment_tombstones = agent_api::attachments::sweep::spawn_tombstones(st.clone());
    // B7 R6: answer and deliver accept_only messages whose edge never asked for the answer (every 30 s)
    let _stuck_accepts = agent_api::conversations::answer_sweeper::spawn(st.clone());
    // SW-11: post queued staff notes to their helpdesk tickets as private comments, retrying while Frappe is down
    let _note_mirrors = agent_api::conversations::note_mirror::spawn(st.clone());
    // SW routing: assign queued handovers round-robin to online staff and move a stale auto-assignment (every 30 s)
    let _support_routing = agent_api::support::routing::spawn(st.clone());
    // B8 T15: purge shadow observations and resume classifying discovery imports left by a previous process
    agent_api::discovery::boot(&st).await;
    agent_api::generator::evidence::boot(&st).await;
    // B8 T12a: grade each approved release's validation cut (approval hook) and resume grid runs left queued
    let _validation_grid = agent_api::validation::grid::install(&st);
    // B8: every 120 s take the grid runs / imports whose lease lapsed (a restarted pod's predecessor), so >1 replica is safe
    let _b8_lease_sweep = agent_api::lease::spawn_sweep(st.clone());
    match &st.signup {
        Some(sg) => sg.log_status(),
        None => tracing::info!("early_access_signup_disabled"),
    }
    // The signup journals (when signup is on) and the licence reconciler (always, D77 L7).
    let _tick = agent_api::signup::provisioning::spawn_tick(st.clone(), st.signup.clone());
    // B7 T2: licence verdicts (60 s + LISTEN nudge), the R3 re-issue reconcile sweep and the R4 pending gauge
    let _verdicts = agent_api::licensing::verdict::spawn_refresher(st.clone());
    // B4 T19: the runtime's Redis-sourced gauges (bus pending/lag, dead letters, breached handovers, heartbeat ages)
    // every 30 s, when REDIS_URL is set
    let _bus_gauges = st
        .redis
        .clone()
        .map(|r| observability::spawn_bus_gauges(st.db.clone(), r));
    let app = router(st);
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    let metrics_listener = tokio::net::TcpListener::bind(&metrics_bind).await?;
    tracing::info!(%bind, %metrics_bind, "agent-api listening");
    // One SIGTERM stops both listeners.
    let (stop_tx, stop_rx) = watch::channel(false);
    tokio::spawn(async move {
        shutdown().await;
        let _ = stop_tx.send(true);
    });
    let api = axum::serve(listener, app).with_graceful_shutdown(stopped(stop_rx.clone()));
    let prom = axum::serve(metrics_listener, observability::metrics_router(metrics))
        .with_graceful_shutdown(stopped(stop_rx));
    tokio::try_join!(api.into_future(), prom.into_future())?;
    Ok(())
}

/// The renotify command (crate::signup::renotify): 0 posted or dry run, 1 usage / refused / not delivered.
async fn renotify(cfg: Config, args: &[String]) -> i32 {
    let apply = args.iter().any(|a| a == "--apply");
    let ids: Vec<&String> = args.iter().filter(|a| !a.starts_with("--")).collect();
    let Some(id) = ids
        .first()
        .and_then(|s| uuid::Uuid::parse_str(s).ok())
        .filter(|_| ids.len() == 1)
    else {
        eprintln!("usage: agent-api renotify-signup <signupId> [--apply]");
        return 1;
    };
    let st = match state::build(cfg).await {
        Ok(st) => st,
        Err(e) => {
            eprintln!("[renotify] could not start: {e:#}");
            return 1;
        }
    };
    match agent_api::signup::renotify::renotify(&st, id, apply).await {
        Ok(Renotice::DryRun(what)) => {
            println!("[renotify] {what}");
            println!("[renotify] DRY RUN: nothing posted. Re-run with --apply.");
            0
        }
        Ok(Renotice::Posted(true)) => {
            println!("[renotify] posted signup {id}'s notice");
            0
        }
        Ok(Renotice::Posted(false)) => {
            eprintln!(
                "[renotify] Telegram did not accept the notice (see the signup_telegram_send log line)"
            );
            1
        }
        Ok(Renotice::Refused(why)) => {
            eprintln!("[renotify] {why}");
            1
        }
        Err(e) => {
            eprintln!("[renotify] failed: {e}");
            1
        }
    }
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
