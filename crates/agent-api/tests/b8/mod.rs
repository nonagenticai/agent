//! B8 (journeys: generator, validation grid, discovery and shadow mode) integration tests as ONE test binary
//! (pattern `tests/b5/main.rs`): one Postgres 18 container for all of B8 (tests/common: one container per binary, a
//! fresh database per test). Run a single module with `cargo test -p agent-api --test all b8::<module>::`.
#![allow(clippy::type_complexity)] // test fixtures read rows as plain tuples

mod designer;
mod discovery_classify;
mod discovery_hygiene;
mod discovery_import;
mod discovery_proposals;
mod draft_validation;
mod generator_drive;
mod generator_llm;
mod generator_runs;
mod job_refine;
mod journey_authoring;
mod journey_grid;
mod kinds;
mod lease_claims;
mod release_backfill;
mod shadow;
mod support;
mod validation_grid;
