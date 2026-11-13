//! B4 (runtime, handover queue, operator console) integration tests as ONE test binary (pattern `tests/b3/main.rs`):
//! every `tests/*.rs` file is its own binary linking every dependency, and the CI runner's 16Gi `work` volume was
//! evicted under that `target/` load. One binary also means one Postgres 18 container for all of B4 (tests/common:
//! one container per binary, a fresh database per test). Run a single module with `cargo test -p agent-api --test all b4::<module>::`.
#![allow(clippy::type_complexity)] // test fixtures read rows as plain tuples

/// The platform content B5 authors on top of the bare offering (activities, spaces, procedures, operations, gates):
/// a B4 test that needs a job with a design uses it rather than a seeded template.
use crate::b5::support as b5_support;
#[allow(dead_code)]
mod support;

mod agent_cases_t6a;
mod assistant;
mod bus;
mod bus_admin;
mod cases;
mod catalog;
mod channels_runtime;
mod claim_stuck;
mod claim_stuck_inflight;
mod conversations;
mod coverage_backfill_strand;
mod customer_context;
mod deadline;
mod design;
mod drain_gate_lock;
mod e2e_m1;
mod events;
mod flow;
mod follow;
mod gap_tests;
mod gate_causes;
mod handover;
mod hours;
mod identity_born;
mod inbox;
mod interaction;
mod it_desk_identity;
mod known_customer_plan;
mod known_records;
mod level_floor;
mod library_flow;
mod metering_drain;
mod migration_order;
mod n12_hand_over;
mod not_eligible_text;
mod op_instance_recorded;
mod operation_calls;
mod orchestrator_notify;
mod out_of_hours_handover;
mod overview;
mod photo_ref;
mod probe_design;
mod probe_turn;
mod publish_coverage_warning;
mod qualify_tenant_provider;
mod rearm_route;
mod resolution;
mod route_health;
mod runner;
mod runner_checks;
mod runner_licence;
mod runner_notify;
mod runtime_role;
mod schema;
mod scope;
mod search;
mod staff_party;
mod staff_test_party;
mod status;
mod step_up_contact;
mod supervisor;
mod suspended_no_new_conversation;
mod usage;
