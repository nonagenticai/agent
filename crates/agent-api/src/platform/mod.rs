//! The platform model (plan B6): layers, row revisions, family tags and their vocabularies. Platform data has no
//! tenant column and no RLS; every function here runs inside the caller's `agent_platform` transaction
//! (crate::platform_db `begin_platform`).
//!
//! - [`bootstrap`]: the platform tenant's identity row (E3), ensured at boot.
//! - [`db`]: the layer store (migrations 0041–0043): revision writes, family tags, readers.
//! - [`hooks`]: the extension points later plans implement (B8 generator guard, B7/B8 object context).
//! - [`routes`]: the B6b platform-model routes (layers, business models, object model, gates), all `SuperAdmin`.
//! - [`selection`]: the hierarchy shape of a stored template selection (`selection_v2`).
//! - [`templates`]: the tag → pin contract (job templates, pinned compile, tag diff, the `job_template_tag`
//!   projection) later plans consume.
pub mod bootstrap;
pub mod db;
pub mod hooks;
pub mod routes;
pub mod selection;
pub mod templates;
