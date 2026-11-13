//! Knowledge (B3): the tenant's store tree bound to strata_rag ([`stores`], T5), and the kind-scoped search over a
//! leaf set with platform packs (R10/R11, PK-7, [`stores::query_leaf_set`]). Later B3 tasks add sources, runs, kinds,
//! coverage and analytics here.
pub mod analytics;
pub mod citations;
pub mod coverage;
pub mod generations;
pub mod kinds;
pub mod model_changes;
pub mod packs;
pub mod readiness;
pub mod runs;
pub mod schedule;
pub mod sources;
pub mod stores;
pub mod targets;
pub mod topic_keys;
