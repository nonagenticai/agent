//! B8 validation (J17): the per-release Q/A grid graded against the release's knowledge with a Laya judge (T12),
//! graded again after every person's approve (F-06, T12a), and its question hygiene (F-50). Draft validation (T13)
//! lands beside it. Routes: crate::routes::validation.
pub mod cuts;
pub mod draft;
pub mod grid;
pub mod judge;
pub mod questions;
