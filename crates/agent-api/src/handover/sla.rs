//! SLA due times and clocks of handover items (D64, B4 T16; `J21Handover.ts` `dueTimes` / `slaOf`).
//!
//! - **Due times** are stamped once, when an item enters the queue ([`stamp_minutes`], called by
//!   [`crate::conversations::handover::create`] in its transaction): a prod item from the job's **live** policy, a
//!   test item from the **draft** (plan R8). A later policy change never moves an item's due times.
//! - **Clocks** ([`clock`]) are read against the database `now()`, the `now` the queue returns (the web computes the
//!   same states from that `now`).
use chrono::{DateTime, Utc};
use sqlx::PgConnection;
use uuid::Uuid;

use super::policy::{self, SlaTarget};
use crate::design::{CaseDesign, DesignOf, JobRef};
use crate::error::ApiResult;

/// Share of a target window left at which a running clock turns `at_risk`.
pub const AT_RISK_SHARE: f64 = 0.25;

/// The design an item's due times come from: the live policy for a prod item, the draft for a test item.
pub fn design_of(d: &CaseDesign) -> DesignOf {
    match d {
        CaseDesign::Live { .. } => DesignOf::Live,
        CaseDesign::Draft { .. } => DesignOf::Draft,
    }
}

/// The targets (minutes) an item of `priority` entering the queue of job `job_id` under `design` is stamped with;
/// `None` = the policy has no SLA.
pub async fn stamp_minutes(
    c: &mut PgConnection,
    job_id: Uuid,
    design: &CaseDesign,
    priority: &str,
) -> ApiResult<Option<SlaTarget>> {
    let key: String = sqlx::query_scalar("SELECT key FROM jobs WHERE id = $1")
        .bind(job_id)
        .fetch_one(&mut *c)
        .await?;
    let job = JobRef { id: job_id, key };
    let targets = policy::read(c, &job, &design_of(design)).await?;
    Ok(targets.and_then(|t| t.of(priority)))
}

/// `none` no target · `running` / `at_risk` open clock · `breached` open past due · `met` / `missed` stopped clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClockState {
    None,
    Running,
    AtRisk,
    Breached,
    Met,
    Missed,
}

impl ClockState {
    pub fn as_str(self) -> &'static str {
        match self {
            ClockState::None => "none",
            ClockState::Running => "running",
            ClockState::AtRisk => "at_risk",
            ClockState::Breached => "breached",
            ClockState::Met => "met",
            ClockState::Missed => "missed",
        }
    }
}

/// Whole minutes from `a` to `b` (`Math.round`).
fn mins(a: DateTime<Utc>, b: DateTime<Utc>) -> i64 {
    ((b - a).num_milliseconds() as f64 / 60_000.0).round() as i64
}

/// One clock (`J21Handover.ts` `clock`): started at `start`, due at `due`, stopped at `stopped` (first response or
/// close), read at `now`.
pub fn clock(
    start: DateTime<Utc>,
    due: Option<DateTime<Utc>>,
    stopped: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> ClockState {
    let Some(due) = due else {
        return ClockState::None;
    };
    if let Some(at) = stopped {
        return if mins(at, due) >= 0 {
            ClockState::Met
        } else {
            ClockState::Missed
        };
    }
    let left = mins(now, due);
    if left < 0 {
        return ClockState::Breached;
    }
    let window = mins(start, due).max(1);
    if (left as f64) <= window as f64 * AT_RISK_SHARE {
        ClockState::AtRisk
    } else {
        ClockState::Running
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    #[test]
    fn clock_follows_j21() {
        let q: DateTime<Utc> = "2026-10-21T10:00:00Z".parse().unwrap();
        let due = q + Duration::minutes(60);
        assert_eq!(clock(q, None, None, q), ClockState::None);
        assert_eq!(
            clock(q, Some(due), None, q + Duration::minutes(10)),
            ClockState::Running
        );
        assert_eq!(
            clock(q, Some(due), None, q + Duration::minutes(45)),
            ClockState::AtRisk
        );
        assert_eq!(
            clock(q, Some(due), None, q + Duration::minutes(61)),
            ClockState::Breached
        );
        assert_eq!(
            clock(q, Some(due), Some(q + Duration::minutes(30)), q),
            ClockState::Met
        );
        assert_eq!(
            clock(q, Some(due), Some(q + Duration::minutes(90)), q),
            ClockState::Missed
        );
        assert_eq!(ClockState::AtRisk.as_str(), "at_risk");
    }
}
