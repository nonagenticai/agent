//! Caller identity / tenant IAM (plan 2026-10-30-caller-identity-iam, Part A) acceptance tests, REAL STACK: Postgres
//! 18, Redis, the API router, and the orchestrator / runner / interaction roles running on the shared bus. The only
//! fakes are at the boundaries: the language model (an OpenAI-compatible server on loopback that records every
//! request body) and the tenant's external system (an HTTP server on loopback behind a real connector instance).
#![allow(dead_code)]
#![allow(clippy::type_complexity)]

mod a2_chain;
mod a2_guards;
mod a2_validation;
mod a3_procedures;
mod a4_callers;
mod a5_policy;
mod a6_identity;
mod b1_b4_mcp;
mod b2_tasks;
mod world;
