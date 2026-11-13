//! J7 gates (B5 T8–T10a): the questions a job asks and how they are answered.
//!
//! - [`library`] (T8, T8a): the base questions at the design's pins, the tenant's refinements and settings, the
//!   effective fold the routes and the runtime share, the rule helpers (`gateRules.ts`) and J7's changes.
//! - [`stats`] (T8): the last 7 days of the gate log per question.
//! - [`laya`], [`evaluate`], [`state`] (T9): the Laya client, the pure evaluator and the per-point state.
//! - [`engine`] (T10): one gate-point run (select, provider, Laya, usage) and the verdict log the runtime writes.
//! - [`route_hint`] (T10a, F-19): the qualifier route hint from P1's GS / GA answers.
//! - [`hooks`] (T12): the in-process P1 / P2 / R1 hooks the runtime runs with (`AppState.gates`, the runner's
//!   `ToolGate`).
pub mod engine;
pub mod evaluate;
pub mod hooks;
pub mod laya;
pub mod library;
pub mod route_hint;
pub mod state;
pub mod stats;
