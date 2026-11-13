//! The `assistant` agent role (B4 T7a, F-40, plan R14): the same runner behind a tool-calling loop. When the
//! qualified context space has no procedure and the role is switched on for the activity, the orchestrator plans
//! the case as an assistant case (`Plan.assistant`, stamped with the same `CaseDesign`) and runs it round by round
//! (crate::runtime::orchestrator):
//!
//! 1. One model call (the job's `qualify` binding, §0 R8; unbound → handover `out_of_scope`) with the tools
//!    [`tools::offered`] grants. Every call writes one `usage_event` row, use label `assistant` (a metering label,
//!    not an LLM use).
//! 2. A reply without tool calls resolves the case (`request.resolved`; the interaction daemon composes the reply as
//!    for any case; the model's answer is the `assistant` fact).
//! 3. Each tool call is an `op.call` (`agent_role` `assistant`, initiator `assistant@<consumer>`) through runner steps
//!    1–11 unchanged — identity level, `confirm_ref` for W/X/P, the 7b per-case cap, `ToolGate`, the route. A
//!    refused or failed call goes back to the model as `{"error": <outcome_code>}` and is never retried; a name the
//!    loop did not offer is answered `not_entitled` without a call. When every call of the round answered, the
//!    next round starts.
//! 4. `unknown_outcome`, the per-case cap and a false postcondition stop the case as handed over (the runner queued
//!    the item). More than [`MAX_TOOL_CALLS`] tool calls, or a turn longer than [`TURN_BUDGET`], hands it over
//!    `tool_failed` ([`BUDGET_EXHAUSTED`]); the case's own deadline (3,600 s) still applies.
//!
//! Off by default: B4 ships [`Disabled`] and [`tools::NoToolset`]; B5 T11a replaces the switch with the job's
//! `role_setting` ∧ the platform `agent_role.enabled` (B6) and the toolset with its `ToolsetIndex`.
pub mod tools;
#[path = "loop.rs"]
pub mod turn;

use std::sync::Arc;
use std::time::Duration;

use self::tools::{NoToolset, ToolsetIndex};
use super::design::RuntimeDesign;
use crate::conversations::store::Conv;
use crate::state::AppState;

/// Tool calls one turn may make.
pub const MAX_TOOL_CALLS: u32 = 6;
/// How long one turn may run (from its first model call). Above one model call ([`turn::LLM_TIMEOUT`], 600 s) so a
/// slow local-llm (~7.5 tok/s, owner 2026-11-03) can finish a round and the turn can still use a tool; below the
/// case deadline. A job's own `turn_budget_s` stays 5..=600 (the column's CHECK).
pub const TURN_BUDGET: Duration = Duration::from_secs(1800);
const _: () = assert!(TURN_BUDGET.as_secs() > turn::LLM_TIMEOUT.as_secs());
const _: () =
    assert!(TURN_BUDGET.as_secs() < crate::runtime::case::DEFAULT_CASE_DEADLINE.as_secs());
/// The outcome of a case whose turn ran out of tool calls or time (handover `tool_failed`).
pub const BUDGET_EXHAUSTED: &str = "assistant_budget_exhausted";
/// The outcome of an assistant case with no usable model (handover `out_of_scope`).
pub const UNBOUND: &str = "assistant_unbound";
/// The outcome of a case whose model did not answer (handover `tool_failed`).
pub const LLM_UNAVAILABLE: &str = "llm_unavailable";
/// The `usage_event.use` metering label of every assistant model call (§0 R8).
pub const USAGE_LABEL: &str = "assistant";

/// Is the `assistant` role on for `activity` of the job? B5 T11a: the job's `role_setting` ∧ the platform
/// `agent_role.enabled`.
pub trait RoleSwitch: Send + Sync {
    fn assistant_enabled(&self, d: &RuntimeDesign, activity: &str) -> bool;
}

/// B4's switch: always off (behaviour stays the procedure path, or the `out_of_scope` handover).
pub struct Disabled;

impl RoleSwitch for Disabled {
    fn assistant_enabled(&self, _d: &RuntimeDesign, _activity: &str) -> bool {
        false
    }
}

/// The turn's budgets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Budget {
    pub max_tool_calls: u32,
    pub turn: Duration,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            max_tool_calls: MAX_TOOL_CALLS,
            turn: TURN_BUDGET,
        }
    }
}

/// What an orchestrator needs to run assistant cases: the state the model resolution, the netguard and metering
/// read (its `db` is not used: the orchestrator's `agent_runtime` pool runs every query), the switch, the toolset
/// and the budgets.
pub struct Assistant {
    st: AppState,
    switch: Arc<dyn RoleSwitch>,
    toolset: Arc<dyn ToolsetIndex>,
    budget: Budget,
}

impl Assistant {
    /// Off ([`Disabled`]), no tools ([`NoToolset`]), the default budgets.
    pub fn new(st: AppState) -> Self {
        Self {
            st,
            switch: Arc::new(Disabled),
            toolset: Arc::new(NoToolset),
            budget: Budget::default(),
        }
    }

    pub fn with_switch(mut self, s: Arc<dyn RoleSwitch>) -> Self {
        self.switch = s;
        self
    }

    pub fn with_toolset(mut self, t: Arc<dyn ToolsetIndex>) -> Self {
        self.toolset = t;
        self
    }

    pub fn with_budget(mut self, b: Budget) -> Self {
        self.budget = b;
        self
    }

    /// The role is on for `activity` (none yet before B5: `""`).
    pub fn enabled(&self, d: &RuntimeDesign, activity: Option<&str>) -> bool {
        self.switch
            .assistant_enabled(d, activity.unwrap_or_default())
    }

    pub fn state(&self) -> &AppState {
        &self.st
    }

    pub fn toolset(&self) -> &dyn ToolsetIndex {
        self.toolset.as_ref()
    }

    pub fn budget(&self) -> Budget {
        self.budget
    }
}

/// The usage `source` and `actor` of a model call in `conv` (as the drain meters the qualifier): a staff test chat
/// is `staff_test` by its staff member, anything else `turn` by `callers:<channel>`.
pub fn usage_party(conv: Option<&Conv>) -> (&'static str, String) {
    match conv {
        Some(c) if c.channel == "staff" => (
            "staff_test",
            c.initiator_sub
                .clone()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "callers:staff".into()),
        ),
        Some(c) => ("turn", format!("callers:{}", c.channel)),
        None => ("turn", "callers:unknown".into()),
    }
}
