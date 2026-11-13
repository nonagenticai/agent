//! Tenant lifecycle (B1 T13): create / import, suspend / resume, offboard ([`lifecycle`]); the routes are
//! crate::routes::tenants. [`offboarder`]: who may call the offboard route. [`app_licence`]: a tenant's own licence
//! token for each platform app (inference-router, strata-rag, strata-ingest).
pub mod app_licence;
pub mod lifecycle;
pub mod offboarder;
