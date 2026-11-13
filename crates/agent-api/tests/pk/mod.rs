//! Platform knowledge (PK) integration tests as ONE test binary (conventions: every `tests/*.rs` file is its own
//! binary, and the CI runner's 16Gi `work` volume is evicted under that load). One binary also means one Postgres 18
//! container for all of PK. Run a single module with `cargo test -p agent-api --test all pk::<module>::`.
#![allow(clippy::type_complexity)] // test fixtures read rows as plain tuples

mod curation;
mod cut;
mod exclusions;
mod query;
mod schema;
mod strata;
mod supersedes;
mod template;
