//! Runner step 8, ROUTE HEALTH (B7 connector-instance health read by B4's runner).
//!
//! **Health source.** `connector_instance.status` (0093), written by B7's contract tests
//! ([`crate::connectors::contract`]): `ok`, `auth_failed`, `invalid_target`, `draft_template`, `untested`. There is no
//! background probe and no circuit breaker; the step never makes a network call and never waits on one. It reads
//! the column, through a per-runner cache.
//!
//! **Fail-open rule (the policy, one place).** Only a POSITIVELY unhealthy state blocks an instance:
//! `auth_failed` and `invalid_target` (the two states the connector executor itself answers
//! `connector_misconfigured` for). Everything else routes: `ok`, `untested` (never probed), `draft_template`, a
//! status this build does not know, an instance row that is not visible, and a failed health read. Unknown is
//! never treated as down: a new, untested or briefly unreadable instance must not take a working route away.
//!
//! **Staleness bound.** A read is reused for at most [`MAX_STALENESS`] (15 s); after that the next call re-reads
//! the row (one primary-key select in a short tenant-scoped transaction). A status change by a contract test is
//! therefore seen within that bound, in both directions. A failed read is not cached.
//!
//! **Fallback.** An unhealthy instance is not called (zero upstream calls). The step answers the same safe outcome
//! as an unrouted operation, state `error` / code `not_offered`, with the staff-facing detail `route_unhealthy: ...`
//! on the `operation_call` row, `reason: route_unhealthy` on its `execute` audit row, and the counter
//! `agent_runner_route_unhealthy_total{status}`. The customer-facing text of `not_offered` never asks for an id.
use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use sqlx::PgConnection;
use uuid::Uuid;

/// The audit `reason`, the detail prefix and the metric name stem of a blocked call.
pub const REASON: &str = "route_unhealthy";
/// The counter of blocked calls, labelled with the instance status that blocked.
pub const METRIC: &str = "agent_runner_route_unhealthy_total";
/// How long one read of an instance's status is reused.
pub const MAX_STALENESS: Duration = Duration::from_secs(15);
/// The cache is cleared when it grows past this many instances (a runner serves one tenant's instances).
const MAX_ENTRIES: usize = 1024;

/// What the health model says about one instance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Health {
    /// Positively healthy (`ok`).
    Healthy,
    /// Never probed, draft, unknown or unreadable: routes (fail open).
    Unknown,
    /// Positively unhealthy: the status that says so (`auth_failed` / `invalid_target`). Never called.
    Unhealthy(&'static str),
}

impl Health {
    /// Maps a `connector_instance.status` (None = no readable row) to its health. THE fail-open rule.
    pub fn of_status(status: Option<&str>) -> Health {
        match status {
            Some("ok") => Health::Healthy,
            Some("auth_failed") => Health::Unhealthy("auth_failed"),
            Some("invalid_target") => Health::Unhealthy("invalid_target"),
            _ => Health::Unknown,
        }
    }

    /// The instance may be called.
    pub fn routable(&self) -> bool {
        !matches!(self, Health::Unhealthy(_))
    }
}

/// The staff-facing detail of a blocked call (no input, output or credential).
pub fn detail(operation: &str, status: &str) -> String {
    format!("{REASON}: the system for {operation} is marked {status}; the call was not made.")
}

/// The per-runner cache of instance health reads. Runs on std's `Mutex`: no guard is ever held across an `.await`.
#[derive(Debug)]
pub struct RouteHealth {
    ttl: Duration,
    seen: Mutex<HashMap<(Uuid, String), (Instant, Health)>>,
}

impl Default for RouteHealth {
    fn default() -> Self {
        Self::new(MAX_STALENESS)
    }
}

impl RouteHealth {
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            seen: Mutex::new(HashMap::new()),
        }
    }

    /// The cached health of `instance` if it was read less than the TTL before `now`.
    pub fn cached(&self, tenant: Uuid, instance: &str, now: Instant) -> Option<Health> {
        let seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        let (at, h) = seen.get(&(tenant, instance.to_owned()))?;
        (now.saturating_duration_since(*at) < self.ttl).then(|| h.clone())
    }

    /// Remember a read made at `now`.
    pub fn store(&self, tenant: Uuid, instance: &str, now: Instant, h: Health) {
        let mut seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        if seen.len() >= MAX_ENTRIES {
            seen.clear();
        }
        seen.insert((tenant, instance.to_owned()), (now, h));
    }

    /// The health of `instance`: cached within the TTL, else one primary-key read on `c` (tenant scoped by RLS).
    /// A failed read answers [`Health::Unknown`] (fail open), logged and not cached.
    pub async fn read(&self, c: &mut PgConnection, tenant: Uuid, instance: &str) -> Health {
        let now = Instant::now();
        if let Some(h) = self.cached(tenant, instance, now) {
            return h;
        }
        match sqlx::query_scalar::<_, String>("SELECT status FROM connector_instance WHERE id = $1")
            .bind(instance)
            .fetch_optional(&mut *c)
            .await
        {
            Ok(status) => {
                let h = Health::of_status(status.as_deref());
                self.store(tenant, instance, now, h.clone());
                h
            }
            Err(e) => {
                tracing::warn!(instance, "route health unreadable, routing anyway: {e}");
                Health::Unknown
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_positively_unhealthy_status_blocks() {
        assert_eq!(Health::of_status(Some("ok")), Health::Healthy);
        assert_eq!(
            Health::of_status(Some("auth_failed")),
            Health::Unhealthy("auth_failed")
        );
        assert_eq!(
            Health::of_status(Some("invalid_target")),
            Health::Unhealthy("invalid_target")
        );
        for unknown in [
            Some("untested"),
            Some("draft_template"),
            Some("something_new"),
            None,
        ] {
            let h = Health::of_status(unknown);
            assert_eq!(h, Health::Unknown, "{unknown:?}");
            assert!(h.routable());
        }
        assert!(Health::Healthy.routable());
        assert!(!Health::Unhealthy("auth_failed").routable());
    }

    #[test]
    fn a_read_is_reused_only_within_the_staleness_bound() {
        let cache = RouteHealth::new(Duration::from_secs(15));
        let t = Uuid::from_u128(1);
        let t0 = Instant::now();
        assert_eq!(cache.cached(t, "crm", t0), None);
        cache.store(t, "crm", t0, Health::Unhealthy("auth_failed"));
        let at = |s| t0 + Duration::from_secs(s);
        assert_eq!(
            cache.cached(t, "crm", at(14)),
            Some(Health::Unhealthy("auth_failed"))
        );
        assert_eq!(
            cache.cached(t, "crm", at(15)),
            None,
            "the bound is exclusive"
        );
        assert_eq!(cache.cached(t, "crm", at(60)), None);
        // another tenant's or another instance's read is never reused
        assert_eq!(cache.cached(Uuid::from_u128(2), "crm", at(1)), None);
        assert_eq!(cache.cached(t, "erp", at(1)), None);
    }

    #[test]
    fn the_detail_names_the_reason_and_no_data() {
        let d = detail("order.read", "auth_failed");
        assert!(d.starts_with("route_unhealthy: "), "{d}");
    }
}
