//! Logs, request ids and Prometheus metrics (B1 X6).
//!
//! - Logs are structured lines (`LOG_FORMAT=json`, the default; `pretty` for a terminal), shipped to Loki by
//!   alloy. Spans are the trace: OTLP export is not wired (Tempo is parked), and the span names and fields
//!   below are the contract a later OTLP layer adds without touching call sites.
//! - Every request passes [`http_layer`]: its id is the inbound `x-request-id` when it is 1..=64 characters of
//!   `[A-Za-z0-9-]`, else a fresh uuidv7; it is echoed on the response and readable anywhere in the request's
//!   task through [`request_id`]. The `http` span carries request_id, method, route (the matched TEMPLATE,
//!   never the raw path), status and latency_ms; a tenant id is recorded on it by the tenant extractor. No
//!   subject, no email address and no query string reach a log line.
//! - Metrics are served by [`metrics_router`] on `METRICS_BIND` (default `0.0.0.0:9098`), never on the API
//!   port. Labels never carry a tenant id or a subject. DB-derived gauges are refreshed by an in-process 30 s
//!   tick ([`spawn_refresh`]), so a scrape never touches the database.
use std::future::Future;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::{MatchedPath, Request};
use axum::http::HeaderValue;
use axum::http::header::CONTENT_TYPE;
use axum::middleware::Next;
use axum::response::Response;
use axum::routing::get;
use metrics_exporter_prometheus::{Matcher, PrometheusBuilder, PrometheusHandle};
use sqlx::PgPool;
use tracing::Instrument;
use uuid::Uuid;

/// `METRICS_BIND` default.
pub const DEFAULT_METRICS_BIND: &str = "0.0.0.0:9098";
pub const REQUEST_ID_HEADER: &str = "x-request-id";
/// How often DB-derived gauges are refreshed.
pub const REFRESH_EVERY: Duration = Duration::from_secs(30);
/// Latency buckets (seconds) of every `*_duration_seconds` histogram.
const BUCKETS: [f64; 12] = [
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogFormat {
    Json,
    Pretty,
}

#[derive(Clone, Debug)]
pub struct ObsConfig {
    /// `LOG_FORMAT=json|pretty` (default json).
    pub log_format: LogFormat,
    /// `METRICS_BIND` (default `0.0.0.0:9098`).
    pub metrics_bind: String,
}

impl Default for ObsConfig {
    fn default() -> Self {
        Self {
            log_format: LogFormat::Json,
            metrics_bind: DEFAULT_METRICS_BIND.into(),
        }
    }
}

tokio::task_local! {
    static REQUEST_ID: String;
}

/// The current request's id (`None` outside a request, e.g. in the signup tick).
pub fn request_id() -> Option<String> {
    REQUEST_ID.try_with(Clone::clone).ok()
}

/// Run `f` with `id` as its request id, as [`http_layer`] does for a request (background work, tests).
pub async fn with_request_id<F: Future>(id: String, f: F) -> F::Output {
    REQUEST_ID.scope(id, f).await
}

static HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();

/// The process's Prometheus recorder, installed on first use (once per process; tests share it).
pub fn metrics_handle() -> PrometheusHandle {
    HANDLE
        .get_or_init(|| {
            let builder = PrometheusBuilder::new()
                .set_buckets_for_metric(Matcher::Suffix("_duration_seconds".into()), &BUCKETS)
                .expect("BUCKETS is not empty");
            let handle = match builder.install_recorder() {
                Ok(h) => h,
                Err(e) => {
                    // Only this module installs a recorder; if another one won, ours renders nothing.
                    tracing::error!(error = %e, "metrics_recorder_not_installed");
                    PrometheusBuilder::new().build_recorder().handle()
                }
            };
            metrics::gauge!(
                "agent_build_info",
                "version" => env!("CARGO_PKG_VERSION"),
                "commit" => build_commit()
            )
            .set(1.0);
            register_counters();
            handle
        })
        .clone()
}

/// Counters an alert reads, with their known label values. A counter otherwise appears only at its first increment,
/// so `rate()` / `increase()` over it has no series before the first incident and an absent series reads as healthy.
/// Each is registered at 0 when the recorder is installed (boot); later increments add to it.
pub const ZERO_COUNTERS: &[(&str, &[(&str, &str)])] = &[
    (
        "agent_runner_route_unhealthy_total",
        &[("status", "auth_failed")],
    ),
    (
        "agent_runner_route_unhealthy_total",
        &[("status", "invalid_target")],
    ),
    ("agent_license_orphan_unrevoked_total", &[]),
    ("agent_licence_parked_held_total", &[]),
    ("agent_licence_revoke_timeouts_total", &[]),
    ("agent_licence_region_unmapped_total", &[]),
    (
        "agent_licence_reconcile_repaired_total",
        &[("holder", "tenant")],
    ),
    (
        "agent_licence_reissue_total",
        &[("holder", "tenant"), ("outcome", "pending")],
    ),
    (
        "agent_licence_reissue_total",
        &[("holder", "member"), ("outcome", "pending")],
    ),
    ("agent_member_seen_errors_total", &[]),
    (
        "agent_member_licence_errors_total",
        &[("code", "licences_unavailable")],
    ),
    (
        "agent_member_licence_errors_total",
        &[("code", "licence_refused")],
    ),
    (
        "agent_member_licence_errors_total",
        &[("code", "licence_stale")],
    ),
    ("agent_member_licence_errors_total", &[("code", "internal")]),
    (
        "agent_tenant_licence_errors_total",
        &[("code", "licences_unavailable")],
    ),
    (
        "agent_tenant_licence_errors_total",
        &[("code", "licence_refused")],
    ),
    (
        "agent_tenant_licence_errors_total",
        &[("code", "licence_stale")],
    ),
    ("agent_audit_write_errors_total", &[("log", "tenant")]),
    ("agent_audit_write_errors_total", &[("log", "platform")]),
    // B3 (G7): strata answered `ambiguous_root` although agent named the root (crate::strata::rag)
    ("agent_strata_ambiguous_root_total", &[("op", "query")]),
    ("agent_strata_ambiguous_root_total", &[("op", "eval_run")]),
];

/// The commit of this image for `agent_build_info{commit}` (B2 T0a): `AGENT_GIT_SHA`, which `Dockerfile.rust` sets from
/// the archetype's `GIT_SHA` build arg; trimmed to the 12-character short sha, or `unknown` when unset or blank.
fn build_commit() -> String {
    commit_label(std::env::var("AGENT_GIT_SHA").ok().as_deref())
}

fn commit_label(v: Option<&str>) -> String {
    match v.map(str::trim) {
        Some(s) if !s.is_empty() && s != "unknown" => s.chars().take(12).collect(),
        _ => "unknown".into(),
    }
}

fn register_counters() {
    for (name, labels) in ZERO_COUNTERS {
        let labels: Vec<metrics::Label> = labels
            .iter()
            .map(|(k, v)| metrics::Label::new(*k, *v))
            .collect();
        metrics::counter!(*name, labels).increment(0);
    }
}

/// The log subscriber (once, at boot) and the metrics recorder.
pub fn init(cfg: &ObsConfig) -> anyhow::Result<PrometheusHandle> {
    let filter =
        || tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    let r = match cfg.log_format {
        LogFormat::Json => tracing_subscriber::fmt()
            .with_env_filter(filter())
            .json()
            .flatten_event(true)
            .with_current_span(true)
            .with_span_list(false)
            .try_init(),
        LogFormat::Pretty => tracing_subscriber::fmt()
            .with_env_filter(filter())
            .try_init(),
    };
    r.map_err(|e| anyhow::anyhow!("install the log subscriber: {e}"))?;
    Ok(metrics_handle())
}

/// `GET /metrics` (Prometheus text format). Served on `METRICS_BIND` only.
pub fn metrics_router(h: PrometheusHandle) -> Router {
    Router::new().route(
        "/metrics",
        get(move || {
            let h = h.clone();
            async move {
                (
                    [(
                        CONTENT_TYPE,
                        HeaderValue::from_static("text/plain; version=0.0.4"),
                    )],
                    h.render(),
                )
            }
        }),
    )
}

/// 1..=64 characters of `[A-Za-z0-9-]`.
fn valid_request_id(s: &str) -> bool {
    (1..=64).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// Request id, `http` span, request metrics. Mounted with `Router::layer`, so the matched route template is
/// known; an unmatched request is labelled `unmatched`.
pub async fn http_layer(req: Request, next: Next) -> Response {
    let start = Instant::now();
    let id = req
        .headers()
        .get(REQUEST_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .filter(|s| valid_request_id(s))
        .map(str::to_string)
        .unwrap_or_else(|| Uuid::now_v7().to_string());
    let method = req.method().as_str().to_string();
    let route = req
        .extensions()
        .get::<MatchedPath>()
        .map(|m| m.as_str().to_string())
        .unwrap_or_else(|| "unmatched".into());
    let span = tracing::info_span!(
        "http",
        request_id = %id,
        method = %method,
        route = %route,
        tenant = tracing::field::Empty,
        status = tracing::field::Empty,
        latency_ms = tracing::field::Empty,
    );
    let mut resp = REQUEST_ID
        .scope(id.clone(), next.run(req).instrument(span.clone()))
        .await;
    let status = resp.status().as_u16();
    let elapsed = start.elapsed();
    span.record("status", u64::from(status));
    span.record(
        "latency_ms",
        u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
    );
    span.in_scope(|| tracing::info!("http_request"));
    metrics::counter!(
        "agent_http_requests_total",
        "method" => method.clone(),
        "route" => route.clone(),
        "status" => status.to_string()
    )
    .increment(1);
    metrics::histogram!(
        "agent_http_request_duration_seconds",
        "method" => method,
        "route" => route
    )
    .record(elapsed.as_secs_f64());
    if let Ok(v) = HeaderValue::from_str(&id) {
        resp.headers_mut().insert(REQUEST_ID_HEADER, v);
    }
    resp
}

fn record_upstream(name: &'static str, op: String, outcome: &'static str, elapsed: Duration) {
    metrics::counter!(
        "agent_upstream_requests_total",
        "upstream" => name,
        "op" => op.clone(),
        "outcome" => outcome
    )
    .increment(1);
    metrics::histogram!(
        "agent_upstream_request_duration_seconds",
        "upstream" => name,
        "op" => op
    )
    .record(elapsed.as_secs_f64());
}

/// Time and count one upstream call: outcome `ok` or `error`.
pub async fn upstream<T, E>(
    name: &'static str,
    op: &'static str,
    f: impl Future<Output = Result<T, E>>,
) -> Result<T, E> {
    let start = Instant::now();
    let r = f.await;
    let outcome = if r.is_ok() { "ok" } else { "error" };
    record_upstream(name, op.to_string(), outcome, start.elapsed());
    r
}

/// The outcome label of an upstream HTTP answer.
pub fn http_outcome(status: u16) -> &'static str {
    match status {
        200..=399 => "ok",
        400..=499 => "4xx",
        _ => "5xx",
    }
}

/// [`upstream`] for a call that yields an HTTP response: outcome `ok` (2xx/3xx), `4xx`, `5xx`, or `error`
/// (transport). `op` is a code constant (bounded label set).
pub async fn upstream_http<E>(
    name: &'static str,
    op: String,
    f: impl Future<Output = Result<reqwest::Response, E>>,
) -> Result<reqwest::Response, E> {
    let start = Instant::now();
    let r = f.await;
    let outcome = match &r {
        Ok(resp) => http_outcome(resp.status().as_u16()),
        Err(_) => "error",
    };
    record_upstream(name, op, outcome, start.elapsed());
    r
}

/// An upstream call's human `what` ("keycloak create user") as an op label ("create_user").
pub fn op_label(prefix: &str, what: &str) -> String {
    what.strip_prefix(prefix)
        .unwrap_or(what)
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect()
}

/// Refresh the DB-derived gauges once (pool occupancy) and run the recorder's upkeep.
pub fn refresh(db: &PgPool, h: &PrometheusHandle) {
    let size = f64::from(db.size());
    let idle = db.num_idle() as f64;
    metrics::gauge!("agent_db_pool_connections", "state" => "idle").set(idle);
    metrics::gauge!("agent_db_pool_connections", "state" => "used").set((size - idle).max(0.0));
    h.run_upkeep();
}

/// [`refresh`] and the outbox / signup-journal gauges (crate::provision::outbox::refresh_gauges) every 30 s; never
/// stops.
pub fn spawn_refresh(db: PgPool, h: PrometheusHandle) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut t = tokio::time::interval(REFRESH_EVERY);
        t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            t.tick().await;
            refresh(&db, &h);
            if let Err(e) = crate::provision::outbox::refresh_gauges(&db).await {
                tracing::warn!(error = %e, "metrics_db_gauges_failed");
            }
        }
    })
}

/// B4 T19: the runtime processes have no metrics port; their numbers live in Redis and agent-api exports them
/// (crate::bus_admin::refresh_gauges: `agent_bus_pending`, `agent_bus_lag`, `agent_dead_letters`,
/// `agent_handover_breached`, `agent_heartbeat_age_seconds`) every [`REFRESH_EVERY`]; never stops. A failed read
/// keeps the last values and is logged.
pub fn spawn_bus_gauges(db: PgPool, root: crate::runtime::Bus) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut t = tokio::time::interval(REFRESH_EVERY);
        t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            t.tick().await;
            if let Err(e) = crate::bus_admin::refresh_gauges(&db, &root).await {
                tracing::warn!(error = %e, "metrics_bus_gauges_failed");
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_label_is_the_short_sha_or_unknown() {
        assert_eq!(commit_label(Some("0123456789abcdef0123")), "0123456789ab");
        assert_eq!(commit_label(Some(" abc ")), "abc");
        assert_eq!(commit_label(Some("")), "unknown");
        assert_eq!(commit_label(Some("unknown")), "unknown");
        assert_eq!(commit_label(None), "unknown");
    }

    #[test]
    fn request_ids_are_bounded_and_plain() {
        assert!(valid_request_id("abc-1"));
        assert!(valid_request_id(&"a".repeat(64)));
        assert!(!valid_request_id(&"a".repeat(65)));
        assert!(!valid_request_id(""));
        assert!(!valid_request_id("a b"));
        assert!(!valid_request_id("a\"b"));
    }

    #[test]
    fn op_labels_are_snake_case_without_the_upstream_prefix() {
        assert_eq!(op_label("keycloak ", "keycloak create user"), "create_user");
        assert_eq!(
            op_label("keycloak ", "keycloak execute-actions-email"),
            "execute_actions_email"
        );
        assert_eq!(op_label("licences ", "licences mint token"), "mint_token");
        assert_eq!(http_outcome(201), "ok");
        assert_eq!(http_outcome(409), "4xx");
        assert_eq!(http_outcome(503), "5xx");
    }
}
