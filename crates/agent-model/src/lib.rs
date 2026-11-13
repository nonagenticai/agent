#![forbid(unsafe_code)]
//! The platform model of agent, pure (no IO): layer keys, the 29 layer-row kinds with their key and field
//! validators, axis conditions, canonical JSON hashing, the layer compiler, catalogue resolution and tag logic.
//! Ported from strata_configurator's `configurator-core` (pin 71aa7af) without the tenant scope (D8).

pub mod axis;
pub mod canonical;
pub mod catalog;
pub mod compile;
pub mod kinds;
pub mod layer;
pub mod rules;
pub mod tags;
