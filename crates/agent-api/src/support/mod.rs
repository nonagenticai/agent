//! The staff workspace (SW plan): what staff read about a conversation, in the same words on every surface.
//!
//! - [`names`]: the customer's display name (never a raw channel id), the channel's label and the plain-language
//!   label of a handover reason (SW T6).
//! - [`inbox`]: `GET /api/support/inbox`, the tenant-wide inbox (SW T7): one set-based read for every row.
//! - [`events`]: `GET /api/support/events`, the tenant-wide live stream (SW T7) over the per-job Redis bridge.
//! - [`presence`]: `POST /api/jobs/{job}/conversations/{id}/presence` (viewing / typing; `viewing` marks it read).
//! - [`canned`]: a job's canned replies (migration 0193).
//! - [`job_staff`]: the per-job staff team and routing setting, `/api/jobs/{job}/staff` (D125, migration 0213).
//! - [`routing`]: round-robin auto-assignment of handovers and its setting, `/api/support/routing` (migration 0204).
//! - [`csat_report`]: `GET /api/support/csat`, the tenant-wide CSAT report (SW-6; `tenant_admin` / `support_manager`).
//! - [`routes`]: the routes of this module.
//!
//! Who may read the workspace: a member holding any of the staff roles that work a conversation
//! (crate::conversations::routes::STAFF_ROLES: `support_agent`, `support_manager`, `operator`, `job_designer`); anyone
//! else is 403 `forbidden` with `missing_role: support_agent`. Roles are tenant-wide (`users.roles`), so a caller who
//! holds one sees the rows of every job of the tenant.
pub mod canned;
pub mod csat_report;
pub mod events;
pub mod inbox;
pub mod job_staff;
pub mod names;
pub mod presence;
pub mod routes;
pub mod routing;

use crate::auth::Role;
use crate::conversations::routes::STAFF_ROLES;
use crate::error::ApiResult;
use crate::tenancy::{TenantCtx, missing_role};

/// 403 `forbidden` (`missing_role: support_agent`) unless the caller's effective roles hold one of the staff roles.
pub(crate) fn require_worker(t: &TenantCtx) -> ApiResult<()> {
    if STAFF_ROLES.iter().any(|r| t.effective.has(*r)) {
        Ok(())
    } else {
        Err(missing_role(Role::SupportAgent))
    }
}
