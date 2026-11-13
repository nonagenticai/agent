//! The handover remainder of B4 T16 (D64) next to B12's queue (crate::conversations::handover):
//!
//! - [`policy`]: the SLA policy, B2 ledger kind `handover_policy` (DesignTarget, working row 0069, validation).
//! - [`sla`]: due times stamped when an item enters the queue (live policy in prod, draft in test) and the clocks.
//! - [`feedback`]: the person's verdict on an item and the draft-proposal refusals.
//! - [`signals`]: what the queue tells discovery (B8).
//! - [`hours`]: the business hours of a policy (T16a, F-14) and the out-of-hours check of `case.create`.
//! - [`calendar`]: the tenant holiday calendar, B2 tenant-scope ledger kind `tenant_calendar` (working row 0066).
//!
//! The routes are crate::routes::handover and crate::routes::tenant_calendar.
pub mod calendar;
pub mod feedback;
pub mod hours;
pub mod policy;
pub mod signals;
pub mod sla;
