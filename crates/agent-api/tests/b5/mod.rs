//! B5 (job design: ontology, context spaces, procedures, gates, toolsets) integration tests as ONE test binary
//! (pattern `tests/b4/main.rs`): one Postgres 18 container for all of B5 (tests/common: one container per binary, a
//! fresh database per test). Run a single module with `cargo test -p agent-api --test all b5::<module>::`.
#![allow(clippy::type_complexity)] // test fixtures read rows as plain tuples

mod base;
mod conditions;
mod design_procedures;
mod gate_settings;
mod gates;
mod gates_eval;
mod laya;
mod ontology;
mod procedures;
mod route_hint;
mod runtime;
mod schema;
pub(crate) mod support;
mod targets;
mod toolsets;
