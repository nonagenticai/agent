//! Connectors (B7, pulled forward 2026-10-20): platform connector templates, the declarative mapping engine,
//! SSRF-guarded transports and tenant connector instances with sealed credentials.
//!
//! Ownership: `wire` is the shared vocabulary; `model`/`opkey`/`jsonpath`/`validate`/`mapping` are pure logic
//! (TS oracle in `web/src/lib/connectors/ops.ts`, golden-tested); `transport` performs calls; `seed`, `custody`
//! and `instances` touch the database; `routes` serves the `/api/connections/…` methods. B7 T13: `resolve` and
//! `readiness` are the pure route resolution and D67 cells (TS oracle, golden-tested), `routes_store` the job route
//! rows and expected operations, `spec_export` the T13a field-spec export. `places` holds B7's three B2 design
//! targets (`route`, `mapping_override`, `tenant_template`) and the only writes to their working rows.
pub mod contract;
pub mod coverage;
pub mod custody;
pub mod diff;
pub mod insight;
pub mod instances;
pub mod jsonpath;
pub mod mapping;
pub mod model;
pub mod opkey;
pub mod overrides;
pub mod places;
pub mod readiness;
pub mod resolve;
pub mod routes;
pub mod routes_store;
pub mod runtime;
pub mod spec_export;
pub mod t1;
pub mod t2;
pub mod tasks;
pub mod tenant_templates;
pub mod transport;
pub mod validate;
pub mod wire;
