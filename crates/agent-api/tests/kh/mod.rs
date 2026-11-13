//! Knowledge hierarchy (plan KH) integration tests as ONE test binary (conventions: every `tests/*.rs` file is its
//! own binary, and the CI runner's 16Gi `work` volume is evicted under that load). One binary also means one
//! Postgres 18 container for all of KH. Run a single module with `--test all kh::<module>::`.
#![allow(clippy::type_complexity)] // test fixtures read rows as plain tuples

mod acceptance;
mod d130_publish;
mod migration_order;
mod t1_selection;
mod t3_fold;
mod t4_instruction;
mod t5_prompt;
mod t6_answer_path;
mod t7_answer;
mod t8_readiness;
mod t9_region;
