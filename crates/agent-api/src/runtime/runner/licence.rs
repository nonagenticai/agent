//! Runner step 2, the licence gate (B5; spec 12 §3.3 / K35): a call of a tenant whose licence is not a fresh `active`
//! verdict is refused `denied` / [`CODE`] before any other check and before any connector is reached, the case hands
//! over (`tool_failed`, outcome [`CODE`]) and the call's `execute` and `operation.denied` audit rows say why.
//!
//! **Where the verdict comes from:** the stored `tenant_licence_verdict` row the agent-api refresher keeps (it never
//! calls licences from here), read through the definer `agent_runtime_licence_verdict()` (0209) in the call's own
//! tenant-scoped transaction, and judged by the same pure rule as gate 2 at the HTTP edge and the drain
//! ([`crate::licensing::gate::decide`]: platform-owned, or a fresh `active`; everything else, a missing row, an outage
//! and a stale `active` included, fails closed). It runs only while `LICENSE_GATE_2_ENABLED` is on, as everywhere.
//!
//! **Cost and staleness:** the runner serves one tenant, so one [`LicenceCache`] slot remembers the last decision for
//! at most [`CACHE_TTL`], and for an `active` verdict no longer than the verdict itself stays fresh
//! ([`MAX_AGE_SECONDS`] minus its age). A licence lapsing or being suspended therefore stops steps within
//! `refresher interval + CACHE_TTL` at worst, the same bound the HTTP edge has plus the TTL, and no step costs a
//! network call; a cache hit costs no query either.
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sqlx::PgConnection;

use crate::error::ApiResult;
use crate::licensing::gate;
use crate::licensing::verdict::{MAX_AGE_SECONDS, Verdict};
use crate::state::AppState;

/// The `denied` outcome code of a call of a tenant that is not licensed.
pub const CODE: &str = "licence_inactive";
/// How long one decision is remembered (spec 03 §2.6: the 30 s TTL backstop of the verdict invalidation).
pub const CACHE_TTL: Duration = Duration::from_secs(30);

/// The handover trigger of a refused call (an operation key, never case data).
pub fn trigger(op_key: &str) -> String {
    format!("licence inactive for {op_key}")
}

/// One remembered decision: admitted, or refused with the gate's state label (`revoked`, `stale`, `missing`, ...).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Decision {
    Admit,
    Refuse(&'static str),
}

/// A monotonic time source.
pub type Clock = Arc<dyn Fn() -> Instant + Send + Sync>;

/// The last decision of the runner's tenant and when it stops counting.
pub struct LicenceCache {
    ttl: Duration,
    /// The time source (tests drive it by hand; production reads the monotonic clock).
    clock: Clock,
    slot: Mutex<Option<(Instant, Decision)>>,
}

impl LicenceCache {
    pub fn new(ttl: Duration) -> Self {
        Self::with_clock(ttl, Arc::new(Instant::now))
    }

    /// A cache whose notion of now is `clock`.
    pub fn with_clock(ttl: Duration, clock: Clock) -> Self {
        Self {
            ttl,
            clock,
            slot: Mutex::new(None),
        }
    }

    fn get(&self, now: Instant) -> Option<Decision> {
        // a poisoned lock only means another thread panicked while holding a plain Option: use it
        let slot = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        slot.filter(|(until, _)| now < *until).map(|(_, d)| d)
    }

    fn put(&self, now: Instant, valid_for: Duration, d: Decision) {
        let mut slot = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        *slot = Some((now + valid_for.min(self.ttl), d));
    }
}

impl std::fmt::Debug for LicenceCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LicenceCache")
            .field("ttl", &self.ttl)
            .finish()
    }
}

impl Default for LicenceCache {
    fn default() -> Self {
        Self::new(CACHE_TTL)
    }
}

/// How long a decision about `v` may be remembered: an `active` verdict only while it is fresh; anything else for a
/// full TTL (the cache caps it).
fn valid_for(v: Option<&Verdict>) -> Duration {
    match v {
        Some(v) if v.status == "active" => {
            Duration::from_secs(u64::try_from(MAX_AGE_SECONDS - v.age_seconds).unwrap_or(0))
        }
        _ => Duration::MAX,
    }
}

/// The caller's tenant verdict through the 0209 definer (`None`: the tenant has no row yet).
async fn read(c: &mut PgConnection, tenant: uuid::Uuid) -> ApiResult<Option<Verdict>> {
    let row: Option<(String, Option<String>, bool, i64)> = sqlx::query_as(
        "SELECT status, reason, platform_owned, age_seconds FROM agent_runtime_licence_verdict()",
    )
    .fetch_optional(&mut *c)
    .await?;
    Ok(
        row.map(|(status, reason, platform_owned, age_seconds)| Verdict {
            tenant_id: tenant,
            status,
            reason,
            licence_id: None,
            plan: None,
            kid: None,
            platform_owned,
            checked_at: chrono::Utc::now() - chrono::Duration::seconds(age_seconds),
            age_seconds,
        }),
    )
}

/// Step 2 for the open tenant-scoped transaction `c` of `tenant`: `Ok(Err(state))` = refuse with [`CODE`] (the state
/// is the gate's label, the call's `detail`). A verdict that cannot be read is an error: the call does not run.
pub async fn admits(
    cache: &LicenceCache,
    st: &AppState,
    c: &mut PgConnection,
    tenant: uuid::Uuid,
) -> ApiResult<Result<(), &'static str>> {
    if !st.cfg.license_gate_2_enabled {
        return Ok(Ok(()));
    }
    let now = (cache.clock)();
    let decision = match cache.get(now) {
        Some(d) => d,
        None => {
            let owned = crate::providers::llm::platform_owned(c, st, tenant).await?;
            let v = if owned { None } else { read(c, tenant).await? };
            let d = if gate::decide(v.as_ref(), owned) {
                Decision::Admit
            } else {
                Decision::Refuse(gate::state_of(v.as_ref()))
            };
            cache.put(now, valid_for(v.as_ref()), d);
            d
        }
    };
    Ok(match decision {
        Decision::Admit => Ok(()),
        Decision::Refuse(state) => {
            gate::refused(tenant, state);
            Err(state)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::licensing::gate::Reading;

    fn v(status: &str, age: i64) -> Verdict {
        Verdict {
            tenant_id: uuid::Uuid::nil(),
            status: status.into(),
            reason: None,
            licence_id: None,
            plan: None,
            kid: None,
            platform_owned: false,
            checked_at: chrono::Utc::now(),
            age_seconds: age,
        }
    }

    #[test]
    fn cache_entry_lives_for_the_shorter_of_ttl_and_verdict_freshness() {
        let c = LicenceCache::new(Duration::from_secs(30));
        let t0 = Instant::now();
        c.put(t0, valid_for(Some(&v("active", 0))), Decision::Admit);
        assert_eq!(c.get(t0 + Duration::from_secs(29)), Some(Decision::Admit));
        assert_eq!(c.get(t0 + Duration::from_secs(30)), None, "the TTL caps it");
        // a verdict 290 s old has 10 s of freshness left: never remembered longer
        c.put(t0, valid_for(Some(&v("active", 290))), Decision::Admit);
        assert_eq!(c.get(t0 + Duration::from_secs(9)), Some(Decision::Admit));
        assert_eq!(c.get(t0 + Duration::from_secs(10)), None);
        // a stale one is not remembered at all
        c.put(
            t0,
            valid_for(Some(&v("active", 400))),
            Decision::Refuse("stale"),
        );
        assert_eq!(c.get(t0), None);
        // a refusal is remembered for the TTL
        c.put(
            t0,
            valid_for(Some(&v("revoked", 0))),
            Decision::Refuse("revoked"),
        );
        assert_eq!(
            c.get(t0 + Duration::from_secs(1)),
            Some(Decision::Refuse("revoked"))
        );
    }

    #[test]
    fn reading_labels_are_the_gates() {
        assert_eq!(
            gate::reading(gate::state_of(Some(&v("active", 0)))),
            Reading::Active
        );
        assert_eq!(
            gate::reading(gate::state_of(Some(&v("active", 301)))),
            Reading::Undecided
        );
    }
}
