//! B3 (knowledge + providers) integration tests as ONE test binary: every `tests/*.rs` file is its own binary
//! linking every dependency, and the CI runner's 16Gi `work` volume was evicted under that `target/` load. One
//! binary also means one Postgres 18 container for all of B3 (tests/common: one container per binary, a fresh
//! database per test). Run a single module with `cargo test -p agent-api --test all b3::<module>::`.
#![allow(clippy::type_complexity)] // test fixtures read rows as plain tuples

mod coverage;
mod eval;
mod foundations;
mod grounding;
mod ingest;
mod knowledge;
mod knowledge_kinds;
mod llm_providers;
mod model_changes;
mod provider_slots;
mod readiness;
mod schedule;
mod schema;
mod source_runs;
mod sources;
mod speech_resolve;
mod strata;
mod support;
mod tabular;
