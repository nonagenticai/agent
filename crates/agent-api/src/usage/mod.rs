//! Usage and metering (B4 T11; port of care-bus-agents@96c608c `metering.rs` + `routes/usage.rs`).
//!
//! - [`emit()`] writes one `usage_event` ledger row (0064) per metered call, always, with the licence attribution of
//!   `agent_licence_attribution()`; with `METERING_EMIT_ENABLED=true` it also writes the row's `metering_outbox` row in
//!   the same transaction.
//! - [`drainer`] sends the outbox to the billing cost stream (`billing:costs` on `METERING_REDIS_URL`), claiming rows
//!   across tenants through `agent_claim_metering` (0129). It runs inside agent-api and is not spawned while emit is
//!   off (K52: off; license-authz meters the LLM URL and both on would bill every call twice).
//! - [`report`] answers `tenantUsage` (`GET /api/tenant/usage`, crate::routes::usage).
use std::time::Duration;

use anyhow::{Context, Result};

pub mod drainer;
pub mod emit;
pub mod report;

pub use crate::turn::qualify::LlmUsage;
pub use emit::{UsageEvent, emit};

/// billing `cost_source`, `service_key` and `app_id` of every cost event (EMITTER_GUIDE.md registry).
pub const COST_SOURCE: &str = "agent";
pub const DEFAULT_COSTS_STREAM: &str = "billing:costs";
pub const DEFAULT_OUTBOX_INTERVAL_MS: u64 = 30_000;

/// The metering switches. `Debug` never prints the Redis URL (it may carry a password).
#[derive(Clone)]
pub struct MeteringConfig {
    /// `METERING_EMIT_ENABLED` (default false): also write the outbox and run the drainer. The ledger is written
    /// regardless.
    pub emit_enabled: bool,
    /// `METERING_REDIS_URL`: the billing cost-bus Redis (not the runtime's `REDIS_URL`). Required when emitting.
    pub redis_url: String,
    /// `METERING_COSTS_STREAM` (default `billing:costs`).
    pub costs_stream: String,
    /// `METERING_OUTBOX_INTERVAL_MS` (default 30 000, at least 1 000): the drainer's tick.
    pub outbox_interval: Duration,
}

impl Default for MeteringConfig {
    /// Off.
    fn default() -> Self {
        Self {
            emit_enabled: false,
            redis_url: String::new(),
            costs_stream: DEFAULT_COSTS_STREAM.into(),
            outbox_interval: Duration::from_millis(DEFAULT_OUTBOX_INTERVAL_MS),
        }
    }
}

impl std::fmt::Debug for MeteringConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MeteringConfig")
            .field("emit_enabled", &self.emit_enabled)
            .field("redis_url", &"[redacted]")
            .field("costs_stream", &self.costs_stream)
            .field("outbox_interval", &self.outbox_interval)
            .finish()
    }
}

impl MeteringConfig {
    /// From the (trimmed, empty = unset) environment. Emitting without `METERING_REDIS_URL` fails the boot by name.
    pub fn from_lookup(trimmed: &impl Fn(&str) -> Option<String>) -> Result<Self> {
        let emit_enabled = crate::config::flag(trimmed, "METERING_EMIT_ENABLED")?;
        let redis_url = trimmed("METERING_REDIS_URL").unwrap_or_default();
        if emit_enabled && redis_url.is_empty() {
            anyhow::bail!(
                "METERING_EMIT_ENABLED=true needs METERING_REDIS_URL (the billing cost-bus Redis)"
            );
        }
        if !redis_url.is_empty() {
            let ok = reqwest::Url::parse(&redis_url).is_ok_and(|u| {
                matches!(u.scheme(), "redis" | "rediss")
                    && u.host_str().is_some_and(|h| !h.is_empty())
            });
            if !ok {
                anyhow::bail!("METERING_REDIS_URL must be a redis://host:port URL");
            }
        }
        let costs_stream =
            trimmed("METERING_COSTS_STREAM").unwrap_or_else(|| DEFAULT_COSTS_STREAM.into());
        let interval = match trimmed("METERING_OUTBOX_INTERVAL_MS") {
            None => DEFAULT_OUTBOX_INTERVAL_MS,
            Some(v) => v.parse::<u64>().ok().filter(|ms| *ms >= 1_000).context(
                "METERING_OUTBOX_INTERVAL_MS must be a number of milliseconds, at least 1000",
            )?,
        };
        Ok(Self {
            emit_enabled,
            redis_url,
            costs_stream,
            outbox_interval: Duration::from_millis(interval),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let vars: Vec<(String, String)> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k: &str| vars.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone())
    }

    #[test]
    fn emit_is_off_by_default_and_needs_a_redis_url() {
        let off = MeteringConfig::from_lookup(&lookup(&[])).unwrap();
        assert!(!off.emit_enabled);
        assert_eq!(off.costs_stream, "billing:costs");
        assert_eq!(off.outbox_interval, Duration::from_millis(30_000));
        let err = MeteringConfig::from_lookup(&lookup(&[("METERING_EMIT_ENABLED", "true")]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("METERING_REDIS_URL"), "{err}");
        let on = MeteringConfig::from_lookup(&lookup(&[
            ("METERING_EMIT_ENABLED", "true"),
            ("METERING_REDIS_URL", "redis://:pw@billing-redis:6379/0"),
        ]))
        .unwrap();
        assert!(on.emit_enabled);
        assert!(!format!("{on:?}").contains("pw@"));
        assert!(
            MeteringConfig::from_lookup(&lookup(&[("METERING_OUTBOX_INTERVAL_MS", "10")])).is_err()
        );
        assert!(
            MeteringConfig::from_lookup(&lookup(&[("METERING_EMIT_ENABLED", "maybe")])).is_err()
        );
    }
}
