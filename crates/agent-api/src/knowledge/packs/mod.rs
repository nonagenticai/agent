//! Platform knowledge packs (plan PK): strata platform packs referenced by template layers (`knowledge_pack` rows,
//! PK-1/PK-2), resolved to leaf ids before a release cut and snapshotted into the release (PK-4..PK-7), and curated
//! by a super_admin on the platform plane (PK-9).
//!
//! - [`template`]: the fold of the pinned tags' `knowledge_pack` rows into a kind's pins (T3).
//! - [`resolve`]: cut-time resolution, the tenant caches and the release snapshot (T4).
//! - [`curation`]: platform curation over strata (T8).
pub mod curation;
pub mod resolve;
pub mod template;
