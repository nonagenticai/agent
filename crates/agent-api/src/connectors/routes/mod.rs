//! The connectors HTTP surface: tenant systems (`/api/connections/…`, [`instances`]), tenant-private templates
//! (`/api/connections/templates…`, `/api/operations?fields=io`, [`tenant_templates`]) and the super_admin platform
//! template reads (`/api/platform/connector-templates…`, [`platform`]), and a job's operations, routes, tools and
//! field-spec export (`/api/jobs/{job}/operations…`, `/api/jobs/{job}/tools`, [`jobs`]), and the job's own
//! operations from the tenant's templates (`/api/jobs/{job}/own-operations…`, [`job_ops`]).
use axum::Router;

use crate::state::AppState;

pub mod instances;
pub mod job_ops;
pub mod jobs;
pub mod mcp_import;
pub mod platform;
pub mod tenant_templates;

pub fn routes() -> Router<AppState> {
    Router::new()
        .merge(instances::routes())
        .merge(job_ops::routes())
        .merge(jobs::routes())
        .merge(mcp_import::routes())
        .merge(platform::routes())
        .merge(tenant_templates::routes())
}
