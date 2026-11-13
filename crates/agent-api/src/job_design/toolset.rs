//! J8 effective toolsets (B5 T11/T11a): the port of the fixture `computeToolset` (`web/src/lib/api/areas/ontology.ts`)
//! as pure Rust, and the ONE toolset source the runtime consults (B4 owner ruling 5, plan R9): the assistant's offer
//! (`runtime::assistant::tools::offered`) and the runner's `ToolGate` (T12) both ask [`DesignToolsets`], which reads
//! the [`Toolsets`] of the case's [`RuntimeDesign`] (`d.job_design.toolsets`).
//!
//! A toolset is one (role, activity): the activity's operations (N2, the folded activity rows' operations at the design's
//! pins, `job_design::base`) plus, for a role with hand-authored grants, the grants the activity does not expect;
//! each row is granted when the role's grants cover it (N3; the `procedure` role derives its grants from the activity)
//! and its side effect is not above the role's `max_side_effect`, then the tenant's narrowings remove rows. N4 (the
//! route) is the overview's ([`ToolsetRoute`](super::types::ToolsetRoute), `routes::job_design::toolsets`); N5
//! (identity level, confirm-back) is the runner's, per call.
//!
//! Roles are `catalogue.agent_roles` at the design's pins (no copy, R1). A role runs when the platform row is
//! enabled AND, for a role a job may configure ([`CONFIGURABLE_ROLES`]: `assistant`, F-40), the job's `role_setting`
//! switches it on (default off). Reasons are the fixture's strings, in its order: role disabled → N2 → N3 → side
//! effect above the max → narrowed.
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use agent_model::kinds::catalog::{Modality, SideEffect};
use agent_model::kinds::role::AgentRoleFields;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::PgConnection;

use super::base::{self, ActivityOp, Bases};
use super::targets::job_ops::JobOp;
use super::types::{CatalogueOperation, Expected};
use super::{catalogue_operations, implicit_read_builtin, op_label, split_op};
use crate::design::{DesignOf, JobRef, Registry, values_of_kind};
use crate::error::ApiResult;
use crate::release::compile::TemplateCompiler;
use crate::runtime::assistant::tools::ToolsetIndex;
use crate::runtime::assistant::turn::{SYSTEM, WORKER_SYSTEM};
use crate::runtime::assistant::{Budget, MAX_TOOL_CALLS, TURN_BUDGET};
use crate::runtime::design::RuntimeDesign;
use crate::runtime::domain::ROLE_PROCEDURE;

/// The `assistant` role (F-40).
pub const ROLE_ASSISTANT: &str = "assistant";
/// The roles a job may switch on or off (422 `role_not_configurable` for any other).
pub const CONFIGURABLE_ROLES: [&str; 1] = [ROLE_ASSISTANT];
/// `max_tool_calls` bounds (0148 CHECK).
pub const MAX_TOOL_CALLS_RANGE: std::ops::RangeInclusive<u32> = 1..=MAX_TOOL_CALLS;
/// `turn_budget_s` bounds (0233 CHECK).
pub const TURN_BUDGET_S_RANGE: std::ops::RangeInclusive<u32> = 5..=600;
/// `system_prompt` length bound in characters (0233 CHECK).
pub const SYSTEM_PROMPT_MAX: usize = 4000;
/// The `jobs.audience` of a customer-care desk (0214); any other audience runs the neutral worker prompt.
pub const AUDIENCE_CUSTOMERS: &str = "customers";

pub const REASON_N2: &str = "Not expected by this activity (N2)";
pub const REASON_N3: &str = "Not granted to this role (N3)";
pub const REASON_NARROWED: &str = "Narrowed by this tenant";
pub const REASON_DISABLED_FOR_JOB: &str = "Role disabled for this job";
pub const REASON_DISABLED_ON_PLATFORM: &str = "Role disabled on the platform";
/// Why the runtime defers to B4 (debug log): the design has no toolset for the role × activity.
pub const UNGOVERNED: &str = "no toolset for this role and activity";

/// Fixture: `Side effect X is above the role’s max Y`.
pub fn reason_above_max(se: SideEffect, max: SideEffect) -> String {
    format!(
        "Side effect {} is above the role’s max {}",
        se.as_str(),
        max.as_str()
    )
}

/// May a job configure `role`?
pub fn configurable(role: &str) -> bool {
    CONFIGURABLE_ROLES.contains(&role)
}

// ---- role settings (F-40) -------------------------------------------------------------------------------------------

/// A job's setting of one role (`job_agent_role_setting`, ledger kind `role_setting`): the value of its place.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleSetting {
    pub enabled: bool,
    /// Tool calls per assistant turn (1..=6); `None` = [`MAX_TOOL_CALLS`].
    pub max_tool_calls: Option<u32>,
    /// A5: seconds one assistant turn may run (5..=600); `None` = [`TURN_BUDGET`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_budget_s: Option<u32>,
    /// A5: the assistant's task prompt (at most [`SYSTEM_PROMPT_MAX`] characters); `None` = the audience's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    /// A5: a turn on a conversation whose latest case is open continues that case.
    #[serde(default, skip_serializing_if = "is_false")]
    pub case_per_thread: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

impl RoleSetting {
    /// The default (no row): off, the default policy.
    pub fn is_default(&self) -> bool {
        !self.enabled
            && self.max_tool_calls.is_none()
            && self.turn_budget_s.is_none()
            && self.system_prompt.is_none()
            && !self.case_per_thread
    }
}

/// Every role's switches in one design: the job's settings and the platform rows' `enabled`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RoleSettings {
    job: BTreeMap<String, RoleSetting>,
    platform: BTreeMap<String, bool>,
}

impl RoleSettings {
    pub fn new(job: BTreeMap<String, RoleSetting>, platform: BTreeMap<String, bool>) -> Self {
        Self { job, platform }
    }

    /// The job's setting row of `role`, if any.
    pub fn setting(&self, role: &str) -> Option<&RoleSetting> {
        self.job.get(role)
    }

    /// The platform `agent_role` row at the design's pins is enabled (unknown role → false).
    pub fn enabled_platform(&self, role: &str) -> bool {
        self.platform.get(role).copied().unwrap_or(false)
    }

    /// The job's switch: a configurable role is off until the job switches it on; any other role has no job switch.
    pub fn enabled_job(&self, role: &str) -> bool {
        if configurable(role) {
            self.job.get(role).is_some_and(|s| s.enabled)
        } else {
            true
        }
    }

    /// The role runs: platform ∧ job.
    pub fn enabled(&self, role: &str) -> bool {
        self.enabled_platform(role) && self.enabled_job(role)
    }

    /// Why the role does not run (platform first), `None` when it does.
    pub fn disabled_reason(&self, role: &str) -> Option<&'static str> {
        if !self.enabled_platform(role) {
            Some(REASON_DISABLED_ON_PLATFORM)
        } else if !self.enabled_job(role) {
            Some(REASON_DISABLED_FOR_JOB)
        } else {
            None
        }
    }

    /// The job's `max_tool_calls` of `role`, else [`MAX_TOOL_CALLS`].
    pub fn max_tool_calls(&self, role: &str) -> u32 {
        self.job
            .get(role)
            .and_then(|s| s.max_tool_calls)
            .filter(|n| MAX_TOOL_CALLS_RANGE.contains(n))
            .unwrap_or(MAX_TOOL_CALLS)
    }

    /// The job's turn time of `role` (`turn_budget_s`), else `default`.
    pub fn turn_budget(&self, role: &str, default: Duration) -> Duration {
        self.job
            .get(role)
            .and_then(|s| s.turn_budget_s)
            .filter(|n| TURN_BUDGET_S_RANGE.contains(n))
            .map_or(default, |n| Duration::from_secs(u64::from(n)))
    }

    /// The assistant's turn budget for this design (B4 [`Budget`], tool calls and time from the job's setting).
    pub fn assistant_budget(&self) -> Budget {
        Budget {
            max_tool_calls: self.max_tool_calls(ROLE_ASSISTANT),
            turn: self.turn_budget(ROLE_ASSISTANT, TURN_BUDGET),
        }
    }

    /// The assistant's task prompt: the job's own, else the customer-care [`SYSTEM`] for a customers-audience job and
    /// the neutral [`WORKER_SYSTEM`] for any other.
    pub fn system_prompt(&self, audience: &str) -> Cow<'_, str> {
        match self
            .job
            .get(ROLE_ASSISTANT)
            .and_then(|s| s.system_prompt.as_deref())
            .filter(|p| !p.trim().is_empty())
        {
            Some(p) => Cow::Borrowed(p),
            None if audience == AUDIENCE_CUSTOMERS => Cow::Borrowed(SYSTEM),
            None => Cow::Borrowed(WORKER_SYSTEM),
        }
    }

    /// Does a turn on a conversation with an open case continue that case (A5)?
    pub fn case_per_thread(&self) -> bool {
        self.job
            .get(ROLE_ASSISTANT)
            .is_some_and(|s| s.case_per_thread)
    }
}

// ---- toolsets -------------------------------------------------------------------------------------------------------

/// One agent role at the design's pins.
#[derive(Clone, Debug, PartialEq)]
pub struct Role {
    pub key: String,
    pub fields: AgentRoleFields,
}

impl Role {
    fn derived(&self) -> bool {
        self.fields.derives_grants()
    }

    /// N3 for `op` / `variant` (fixture: derived → true; else a grant of the op with no variant or the same one).
    fn grants(&self, op: &str, variant: Option<&str>) -> bool {
        self.derived()
            || self
                .fields
                .grants
                .iter()
                .any(|g| g.op() == op && (g.variant.is_none() || g.variant.as_deref() == variant))
    }
}

/// One row of a toolset before N4 (fixture `ToolsetRow` without `route`).
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    /// `object.verb`
    pub op: String,
    pub variant: Option<String>,
    /// `object.verb{variant}` (web `opLabel`): what a narrowing names.
    pub label: String,
    pub side_effect: SideEffect,
    pub modality: Modality,
    pub expected: Option<Expected>,
    pub in_grant: bool,
    pub above_max: bool,
    /// N3, before the tenant narrowing: in the grant and not above the max.
    pub granted: bool,
    pub narrowed: bool,
    /// Why the row is not effective (before N4), `None` when it is.
    pub reason: Option<String>,
}

/// What [`Toolsets::rows`] could not find.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Missing {
    Role,
    Activity,
}

/// May the role run the operation (the runtime's question, N2 ∩ N3 − narrowings, role enabled)?
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Decision {
    pub granted: bool,
    /// Why not (the fixture's strings); `None` when granted.
    pub reason: Option<String>,
}

/// One design's toolsets: its roles, activities (with their operations), operations, narrowings and role settings.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Toolsets {
    roles: Vec<Role>,
    /// Activity key → its operations (N2), in bundle order.
    activities: Vec<(String, Vec<ActivityOp>)>,
    operations: Vec<CatalogueOperation>,
    /// `(role, activity, op label)`.
    narrowings: BTreeSet<(String, String, String)>,
    pub settings: RoleSettings,
    /// The job's own operations (0246, `object.verb`): the assistant role may run them for every activity, up to its
    /// max side effect (narrowings still apply).
    own: BTreeSet<String>,
}

impl Toolsets {
    /// The toolsets of `bases` with the design's `narrowings` and the job's role `settings`. A role row whose fields
    /// do not parse is left out (it grants nothing), logged.
    pub fn build(
        bases: &Bases,
        narrowings: impl IntoIterator<Item = (String, String, String)>,
        settings: BTreeMap<String, RoleSetting>,
    ) -> Toolsets {
        let roles: Vec<Role> = bases
            .agent_roles
            .iter()
            .filter_map(|item| {
                match serde_json::from_value::<AgentRoleFields>(Value::Object(item.fields.clone()))
                {
                    Ok(fields) => Some(Role {
                        key: item.key.clone(),
                        fields,
                    }),
                    Err(e) => {
                        tracing::warn!(role = %item.key, "agent role row unreadable: {e}");
                        None
                    }
                }
            })
            .collect();
        let platform = roles
            .iter()
            .map(|r| (r.key.clone(), r.fields.enabled))
            .collect();
        Toolsets {
            activities: bases
                .activities
                .iter()
                .map(|a| (a.key.clone(), a.ops.clone()))
                .collect(),
            operations: catalogue_operations(&bases.operations),
            narrowings: narrowings.into_iter().collect(),
            settings: RoleSettings::new(settings, platform),
            roles,
            own: BTreeSet::new(),
        }
    }

    /// These toolsets with the job's own operations `ops` (op key → value, `job_operation`). An op the template's
    /// catalogue already has keeps the catalogue's attributes.
    pub fn with_own(mut self, ops: &BTreeMap<String, JobOp>) -> Toolsets {
        let rows: Vec<_> = ops.iter().map(|(k, j)| j.row(k)).collect();
        for o in catalogue_operations(&rows) {
            if !self
                .operations
                .iter()
                .any(|x| x.op == o.op && x.variant == o.variant)
            {
                self.own.insert(o.op.clone());
                self.operations.push(o);
            }
        }
        self
    }

    pub fn roles(&self) -> &[Role] {
        &self.roles
    }

    pub fn role(&self, key: &str) -> Option<&Role> {
        self.roles.iter().find(|r| r.key == key)
    }

    /// The activity keys, in bundle order.
    pub fn activities(&self) -> impl Iterator<Item = &str> {
        self.activities.iter().map(|(k, _)| k.as_str())
    }

    /// The grant of `activity` (N2): its operations as `object.verb{variant}` labels, the grant publish checks a
    /// step against (`context_space::activity_ops`, the same fold); empty when the design does not know it.
    pub fn activity_grant(&self, activity: &str) -> Vec<String> {
        self.activity(activity)
            .map(|ops| {
                ops.iter()
                    .map(|o| op_label(&o.op, o.variant.as_deref()))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn activity(&self, key: &str) -> Option<&[ActivityOp]> {
        self.activities
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, ops)| ops.as_slice())
    }

    /// Does this design have a toolset for `role` × `activity` (a known role and a known activity)? Without one the
    /// job design has nothing to narrow: the runtime defers to B4's authorisation ([`DesignToolsets`]).
    pub fn governs(&self, role: &str, activity: Option<&str>) -> bool {
        match activity.filter(|a| !a.is_empty()) {
            Some(a) => self.role(role).is_some() && self.activity(a).is_some(),
            None => false,
        }
    }

    /// Does this design have at least one toolset for `role` (a known role and at least one activity)? Such a role
    /// is governed for EVERY call: one with no activity, or an activity the design does not know, is refused
    /// ([`DesignToolsets`], fail-closed). A role the design does not know, or a design without activities, has
    /// nothing to govern and defers to B4.
    pub fn governs_role(&self, role: &str) -> bool {
        self.role(role).is_some() && !self.activities.is_empty()
    }

    pub fn is_narrowed(&self, role: &str, activity: &str, op: &str) -> bool {
        self.narrowings
            .contains(&(role.to_owned(), activity.to_owned(), op.to_owned()))
    }

    /// The narrowings, `(role, activity, op label)`, sorted.
    pub fn narrowings(&self) -> impl Iterator<Item = &(String, String, String)> {
        self.narrowings.iter()
    }

    /// The catalogue operation of `op` / `variant`: the exact variant, else the first of the op (fixture
    /// `catalogueOp`, which looks up by op).
    fn catalogue_op(&self, op: &str, variant: Option<&str>) -> Option<&CatalogueOperation> {
        self.operations
            .iter()
            .find(|o| o.op == op && o.variant.as_deref() == variant)
            .or_else(|| self.operations.iter().find(|o| o.op == op))
    }

    /// The toolset of `role` for `activity` (fixture `computeToolset` without N4).
    pub fn rows(&self, role: &str, activity: &str) -> Result<Vec<Row>, Missing> {
        let r = self.role(role).ok_or(Missing::Role)?;
        let ops = self.activity(activity).ok_or(Missing::Activity)?;
        let mut entries: Vec<(String, Option<String>, Option<Expected>)> = ops
            .iter()
            .map(|x| {
                (
                    x.op.clone(),
                    x.variant.clone(),
                    Some(if x.required { Expected::R } else { Expected::O }),
                )
            })
            .collect();
        if !r.derived() {
            for g in &r.fields.grants {
                let k = g.op();
                if !entries.iter().any(|(op, _, _)| *op == k) {
                    entries.push((k, g.variant.clone(), None));
                }
            }
        }
        // 0246: the job's own operations are the assistant's for every activity
        let own_role = role == ROLE_ASSISTANT;
        if own_role {
            for k in &self.own {
                match entries.iter_mut().find(|(op, _, _)| op == k) {
                    Some(e) => {
                        e.2.get_or_insert(Expected::O);
                    }
                    None => entries.push((k.clone(), None, Some(Expected::O))),
                }
            }
        }
        let disabled = self.settings.disabled_reason(role);
        Ok(entries
            .into_iter()
            .map(|(op, variant, expected)| {
                let cat = self.catalogue_op(&op, variant.as_deref());
                let se = cat.map_or(SideEffect::R, |c| c.side_effect);
                let label = op_label(&op, variant.as_deref());
                let in_grant =
                    r.grants(&op, variant.as_deref()) || (own_role && self.own.contains(&op));
                let above_max = se.rank() > r.fields.max_side_effect.rank();
                let narrowed = self.is_narrowed(role, activity, &label);
                let reason = if let Some(d) = disabled {
                    Some(d.to_owned())
                } else if expected.is_none() {
                    Some(REASON_N2.to_owned())
                } else if !in_grant {
                    Some(REASON_N3.to_owned())
                } else if above_max {
                    Some(reason_above_max(se, r.fields.max_side_effect))
                } else if narrowed {
                    Some(REASON_NARROWED.to_owned())
                } else {
                    None
                };
                Row {
                    modality: cat.map_or(Modality::Observe, |c| c.modality),
                    side_effect: se,
                    granted: in_grant && !above_max,
                    op,
                    variant,
                    label,
                    expected,
                    in_grant,
                    above_max,
                    narrowed,
                    reason,
                }
            })
            .collect())
    }

    /// May `role` run operation `op_key` (`object.verb` / `object.verb{variant}`, `{*}` = no variant) for
    /// `activity`? The row whose label is the key, else the row of the op without a variant (it covers every
    /// variant, web `grantCovers`). No activity, an unknown one, or an op the activity does not expect → N2.
    pub fn decide(&self, role: &str, activity: Option<&str>, op_key: &str) -> Decision {
        let deny = |reason: &str| Decision {
            granted: false,
            reason: Some(reason.to_owned()),
        };
        let Some(r) = self.role(role) else {
            return deny(REASON_N3);
        };
        if let Some(d) = self.settings.disabled_reason(&r.key) {
            return deny(d);
        }
        let Some(activity) = activity else {
            return deny(REASON_N2);
        };
        let Ok(rows) = self.rows(role, activity) else {
            return deny(REASON_N2);
        };
        let key = op_key.strip_suffix("{*}").unwrap_or(op_key);
        let (op, _) = split_op(key);
        let expected = |row: &&Row| row.expected.is_some();
        let row = rows
            .iter()
            .filter(expected)
            .find(|row| row.label == key)
            .or_else(|| {
                rows.iter()
                    .filter(expected)
                    .find(|row| row.op == op && row.variant.is_none())
            });
        match row {
            // The procedure role may run read-only knowledge and rule builtins on a known activity.
            // ([`implicit_read_builtin`]); a tenant narrowing still removes them
            None if role == ROLE_PROCEDURE
                && implicit_read_builtin(key)
                && !self.is_narrowed(role, activity, key)
                && !self.is_narrowed(role, activity, op) =>
            {
                Decision {
                    granted: true,
                    reason: None,
                }
            }
            None => deny(REASON_N2),
            Some(row) => Decision {
                granted: row.reason.is_none(),
                reason: row.reason.clone(),
            },
        }
    }
}

// ---- the one source (B4 owner ruling 5) -----------------------------------------------------------------------------

/// The runtime's toolset index. The ONLY toolset reader of the runtime: the assistant's offer and the runner's
/// `ToolGate` (T12) both call it, so they cannot disagree. A (role, activity) the job design governs
/// ([`Toolsets::governs`]) is `decide(..).granted`. A role the design has toolsets for ([`Toolsets::governs_role`])
/// but called with no activity, or one the design has no toolset for, is refused (fail-closed). A role the design
/// has no toolset for at all has nothing to narrow and defers to B4's own authorisation (allowed, logged at debug).
#[derive(Clone, Copy, Debug, Default)]
pub struct DesignToolsets;

impl DesignToolsets {
    /// Does the job design govern `role` × `activity`? (`None` = defer to B4.)
    pub fn governed(&self, d: &RuntimeDesign, role: &str, activity: Option<&str>) -> bool {
        d.job_design.toolsets.governs(role, activity)
    }

    /// The grant of `activity` in design `d` ([`Toolsets::activity_grant`]); empty for an empty or unknown
    /// activity. B4's `DesignProcedures` checks a designed procedure's steps against it at plan time.
    pub fn activity_grant(&self, d: &RuntimeDesign, activity: &str) -> Vec<String> {
        if activity.is_empty() {
            return Vec::new();
        }
        d.job_design.toolsets.activity_grant(activity)
    }

    /// The full decision (reason included), for the overview and the verdict log.
    pub fn decide(
        &self,
        d: &RuntimeDesign,
        role: &str,
        activity: Option<&str>,
        op: &str,
    ) -> Decision {
        d.job_design.toolsets.decide(role, activity, op)
    }
}

impl ToolsetIndex for DesignToolsets {
    fn allows(&self, d: &RuntimeDesign, role: &str, activity: Option<&str>, op: &str) -> bool {
        if !self.governed(d, role, activity) {
            if d.job_design.toolsets.governs_role(role) {
                // fail-closed: the design governs this role's toolsets, so a call outside any of them is not entitled
                tracing::warn!(
                    role,
                    activity = activity.unwrap_or("-"),
                    op,
                    "toolset_refused_no_known_activity"
                );
                return false;
            }
            tracing::debug!(
                role,
                activity = activity.unwrap_or("-"),
                op,
                reason = UNGOVERNED,
                "toolset_deferred"
            );
            return true;
        }
        self.decide(d, role, activity, op).granted
    }
}

// ---- reading a design -----------------------------------------------------------------------------------------------

/// `(role, activity, op)` of every `narrowing` place of design `of`.
pub async fn narrowings_of(
    c: &mut PgConnection,
    reg: &Registry,
    job: &JobRef,
    of: &DesignOf,
) -> ApiResult<Vec<(String, String, String)>> {
    let field = |r: &serde_json::Map<String, Value>, f: &str| {
        r.get(f).and_then(Value::as_str).unwrap_or("").to_owned()
    };
    Ok(values_of_kind(&mut *c, reg, job, of, "narrowing")
        .await?
        .into_iter()
        .filter(|row| row.snap.as_ref().is_some_and(|v| !v.is_null()))
        .map(|row| {
            (
                field(&row.r#ref, "role"),
                field(&row.r#ref, "activity"),
                field(&row.r#ref, "op"),
            )
        })
        .collect())
}

/// The job's role settings of design `of` (every `role_setting` place). A value of another shape is left out
/// (logged): the role then has the default (off).
pub async fn role_settings_of(
    c: &mut PgConnection,
    reg: &Registry,
    job: &JobRef,
    of: &DesignOf,
) -> ApiResult<BTreeMap<String, RoleSetting>> {
    let mut out = BTreeMap::new();
    for row in values_of_kind(&mut *c, reg, job, of, "role_setting").await? {
        let Some(role) = row.r#ref.get("role").and_then(Value::as_str) else {
            continue;
        };
        let Some(v) = row.snap.filter(|v| !v.is_null()) else {
            continue;
        };
        match serde_json::from_value::<RoleSetting>(v) {
            Ok(s) => {
                out.insert(role.to_owned(), s);
            }
            Err(e) => tracing::warn!(role, "role_setting value unreadable: {e}"),
        }
    }
    Ok(out)
}

/// The toolsets of design `of` (`Live` / `Draft` / `Release`): its bases at its pins (`job_design::base::at`) and its
/// `narrowing` and `role_setting` places — the same read the routes and the runtime (T12) use.
pub async fn of_design(
    c: &mut PgConnection,
    reg: &Registry,
    compiler: &dyn TemplateCompiler,
    job: &JobRef,
    of: &DesignOf,
) -> ApiResult<(Arc<Bases>, Toolsets)> {
    let bases = base::at(&mut *c, reg, compiler, job, of).await?;
    let narrowings = narrowings_of(&mut *c, reg, job, of).await?;
    let settings = role_settings_of(&mut *c, reg, job, of).await?;
    let own = super::targets::job_ops::of_design(&mut *c, reg, job, of).await?;
    let t = Toolsets::build(&bases, narrowings, settings).with_own(&own);
    Ok((bases, t))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job_design::base::BaseActivity;
    use agent_model::compile::CatalogueItem;
    use serde_json::json;

    fn role(key: &str, fields: Value) -> CatalogueItem {
        CatalogueItem {
            key: key.into(),
            row_id: "r".into(),
            layer: "base".into(),
            fields: fields.as_object().cloned().unwrap(),
        }
    }

    fn op(op: &str, variant: Option<&str>, required: bool) -> ActivityOp {
        ActivityOp {
            op: op.into(),
            variant: variant.map(str::to_owned),
            required,
        }
    }

    /// procedure (derived, max P), qualifier (grants order.read, max R), assistant (grants order.read +
    /// order.cancel, max W, platform `enabled`), one activity `returns` (order.read R, case.create{return} O,
    /// payment.refund O).
    fn bases(assistant_on_platform: bool) -> Bases {
        Bases {
            agent_roles: vec![
                role(
                    "procedure",
                    json!({"label": "Procedure", "max_side_effect": "P", "enabled": true,
                       "grants": [{"object": "*", "verb": "*", "variant": null, "modalities": ["do"],
                                   "derive_from_activity": true}]}),
                ),
                role(
                    "qualifier",
                    json!({"label": "Qualifier", "max_side_effect": "R", "enabled": true,
                       "grants": [{"object": "order", "verb": "read", "modalities": ["observe"]},
                                  {"object": "customer", "verb": "read", "modalities": ["observe"]}]}),
                ),
                role(
                    "assistant",
                    json!({"label": "Assistant", "max_side_effect": "W", "enabled": assistant_on_platform,
                       "grants": [{"object": "order", "verb": "read", "modalities": ["observe"]},
                                  {"object": "case", "verb": "create", "variant": "return", "modalities": ["do"]},
                                  {"object": "payment", "verb": "refund", "modalities": ["do"]}]}),
                ),
            ],
            activities: vec![BaseActivity {
                key: "returns".into(),
                item: serde_json::Map::new(),
                ops: vec![
                    op("order.read", None, true),
                    op("case.create", Some("return"), false),
                    op("payment.refund", None, false),
                ],
            }],
            ..Default::default()
        }
    }

    fn assistant_on() -> BTreeMap<String, RoleSetting> {
        BTreeMap::from([(
            ROLE_ASSISTANT.to_owned(),
            RoleSetting {
                enabled: true,
                max_tool_calls: Some(3),
                turn_budget_s: None,
                system_prompt: None,
                case_per_thread: false,
            },
        )])
    }

    fn job_op(se: crate::connectors::model::SideEffect) -> JobOp {
        JobOp {
            identity_level: agent_model::kinds::catalog::IdentityLevel::Public,
            side_effect: se,
            input_fields: vec!["title".into()],
            output_fields: Vec::new(),
            description: String::new(),
            template: "fr_mcp@1".into(),
            input_types: BTreeMap::new(),
        }
    }

    /// 0246: a job's own op is the assistant's for every activity, up to its max side effect; a narrowing still
    /// removes it, and no other role gets it.
    #[test]
    fn a_job_op_is_granted_to_the_assistant_only_up_to_its_max() {
        use crate::connectors::model::SideEffect as Se;
        let own = BTreeMap::from([
            ("fr.start_project".to_owned(), job_op(Se::W)),
            ("erp.pay".to_owned(), job_op(Se::P)),
        ]);
        let t = Toolsets::build(&bases(true), [], assistant_on()).with_own(&own);
        let d = t.decide(ROLE_ASSISTANT, Some("returns"), "fr.start_project");
        assert!(d.granted, "{d:?}");
        let pay = t.decide(ROLE_ASSISTANT, Some("returns"), "erp.pay");
        assert!(!pay.granted, "above the assistant's max W");
        assert!(
            !t.decide("qualifier", Some("returns"), "fr.start_project")
                .granted
        );
        let narrowed = Toolsets::build(
            &bases(true),
            [(
                ROLE_ASSISTANT.to_owned(),
                "returns".to_owned(),
                "fr.start_project".to_owned(),
            )],
            assistant_on(),
        )
        .with_own(&own);
        assert!(
            !narrowed
                .decide(ROLE_ASSISTANT, Some("returns"), "fr.start_project")
                .granted
        );
    }

    #[test]
    fn procedure_role_grant_is_activity_ops() {
        let t = Toolsets::build(&bases(false), [], BTreeMap::new());
        let rows = t.rows("procedure", "returns").unwrap();
        let labels: Vec<&str> = rows.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(
            labels,
            ["order.read", "case.create{return}", "payment.refund"]
        );
        assert!(rows.iter().all(|r| r.granted && r.reason.is_none()));
        assert_eq!(rows[0].expected, Some(Expected::R));
        assert_eq!(rows[1].expected, Some(Expected::O));
    }

    #[test]
    fn qualifier_grants_listed_plus_not_expected_reason() {
        let t = Toolsets::build(&bases(false), [], BTreeMap::new());
        let rows = t.rows("qualifier", "returns").unwrap();
        let customer = rows.iter().find(|r| r.label == "customer.read").unwrap();
        assert_eq!(customer.expected, None);
        assert_eq!(customer.reason.as_deref(), Some(REASON_N2));
        let read = rows.iter().find(|r| r.label == "order.read").unwrap();
        assert!(read.granted && read.reason.is_none());
        let case = rows
            .iter()
            .find(|r| r.label == "case.create{return}")
            .unwrap();
        assert_eq!(case.reason.as_deref(), Some(REASON_N3));
    }

    #[test]
    fn above_max_side_effect_not_granted() {
        let mut b = bases(true);
        b.operations = vec![agent_model::catalog::OperationRow {
            attrs: json!({"side_effect": "P", "modality": "do", "identity_level": "2"})
                .as_object()
                .cloned()
                .unwrap(),
            key: "payment.refund".into(),
            object: "payment".into(),
            verb: "refund".into(),
            variant: agent_model::catalog::STAR.into(),
            discriminator: None,
            overridden: Vec::new(),
            inherited: Vec::new(),
            description: String::new(),
            retail: None,
            stages: Vec::new(),
            error_states: Vec::new(),
        }];
        let t = Toolsets::build(&b, [], assistant_on());
        let rows = t.rows("assistant", "returns").unwrap();
        let refund = rows.iter().find(|r| r.label == "payment.refund").unwrap();
        assert_eq!(refund.side_effect, SideEffect::P);
        assert_eq!(refund.modality, Modality::Do);
        assert!(refund.in_grant && refund.above_max && !refund.granted);
        assert_eq!(
            refund.reason.as_deref(),
            Some("Side effect P is above the role’s max W")
        );
        // an unknown operation is a read (fixture `cat?.side_effect ?? 'R'`)
        let read = rows.iter().find(|r| r.label == "order.read").unwrap();
        assert_eq!(read.side_effect, SideEffect::R);
        assert_eq!(
            reason_above_max(SideEffect::X, SideEffect::R),
            "Side effect X is above the role’s max R"
        );
    }

    #[test]
    fn assistant_disabled_reason() {
        let t = Toolsets::build(&bases(false), [], assistant_on());
        let rows = t.rows("assistant", "returns").unwrap();
        assert!(
            rows.iter()
                .all(|r| r.reason.as_deref() == Some(REASON_DISABLED_ON_PLATFORM))
        );
        let t = Toolsets::build(&bases(true), [], BTreeMap::new());
        let rows = t.rows("assistant", "returns").unwrap();
        assert!(
            rows.iter()
                .all(|r| r.reason.as_deref() == Some(REASON_DISABLED_FOR_JOB))
        );
        assert!(!t.decide("assistant", Some("returns"), "order.read").granted);
    }

    #[test]
    fn enabled_assistant_toolset_is_grants_minus_narrowings() {
        let narrowed = [(
            "assistant".to_owned(),
            "returns".to_owned(),
            "case.create{return}".to_owned(),
        )];
        let t = Toolsets::build(&bases(true), narrowed, assistant_on());
        assert!(t.settings.enabled(ROLE_ASSISTANT));
        let granted: Vec<String> = t
            .rows("assistant", "returns")
            .unwrap()
            .into_iter()
            .filter(|r| r.reason.is_none())
            .map(|r| r.label)
            .collect();
        assert_eq!(granted, ["order.read", "payment.refund"]);
        let d = t.decide("assistant", Some("returns"), "case.create{return}");
        assert_eq!(d.reason.as_deref(), Some(REASON_NARROWED));
        // the narrowing is the assistant's only
        assert!(
            t.decide("procedure", Some("returns"), "case.create{return}")
                .granted
        );
    }

    #[test]
    fn pure_rule_builtin_obeys_role_activity_and_narrowings() {
        let t = Toolsets::build(&bases(true), [], assistant_on());
        assert!(
            t.decide("procedure", Some("returns"), "rule.evaluate{CREDIT}")
                .granted
        );
        assert!(
            !t.decide("assistant", Some("returns"), "rule.evaluate{CREDIT}")
                .granted
        );
        assert!(
            !t.decide("procedure", Some("unknown"), "rule.evaluate{CREDIT}")
                .granted
        );
        for key in ["rule.evaluate", "rule.evaluate{CREDIT}"] {
            let narrowed = [("procedure".to_owned(), "returns".to_owned(), key.to_owned())];
            let t = Toolsets::build(&bases(true), narrowed, assistant_on());
            assert!(
                !t.decide("procedure", Some("returns"), "rule.evaluate{CREDIT}")
                    .granted
            );
        }
    }

    #[test]
    fn only_a_known_role_and_activity_is_governed() {
        let t = Toolsets::build(&bases(true), [], assistant_on());
        assert!(t.governs("procedure", Some("returns")));
        assert!(t.governs("assistant", Some("returns")));
        assert!(!t.governs("procedure", None));
        assert!(!t.governs("procedure", Some("")));
        assert!(!t.governs("procedure", Some("nope")));
        assert!(!t.governs("nobody", Some("returns")));
        assert!(!Toolsets::default().governs("procedure", Some("returns")));
    }

    #[test]
    fn a_role_with_toolsets_is_governed_for_every_activity() {
        let t = Toolsets::build(&bases(true), [], assistant_on());
        assert!(t.governs_role("procedure"));
        assert!(t.governs_role("qualifier"));
        assert!(t.governs_role("assistant"));
        // a role the design does not know defers
        assert!(!t.governs_role("nobody"));
        // no toolset at all (no activity, no role): nothing to govern
        assert!(!Toolsets::default().governs_role("procedure"));
        let mut no_activity = bases(true);
        no_activity.activities.clear();
        let t = Toolsets::build(&no_activity, [], assistant_on());
        assert!(!t.governs_role("procedure"));
    }

    #[test]
    fn decide_matches_variants_and_refuses_the_unexpected() {
        let t = Toolsets::build(&bases(true), [], assistant_on());
        assert!(t.decide("procedure", Some("returns"), "order.read").granted);
        assert!(
            t.decide("procedure", Some("returns"), "order.read{*}")
                .granted
        );
        // `order.read` (no variant) covers every variant
        assert!(
            t.decide("procedure", Some("returns"), "order.read{by_ref}")
                .granted
        );
        // `case.create{return}` does not cover another variant
        let d = t.decide("procedure", Some("returns"), "case.create{complaint}");
        assert_eq!(d.reason.as_deref(), Some(REASON_N2));
        assert_eq!(
            t.decide("procedure", None, "order.read").reason.as_deref(),
            Some(REASON_N2)
        );
        assert_eq!(
            t.decide("procedure", Some("nope"), "order.read")
                .reason
                .as_deref(),
            Some(REASON_N2)
        );
        assert!(!t.decide("nobody", Some("returns"), "order.read").granted);
        assert_eq!(t.rows("nobody", "returns"), Err(Missing::Role));
        assert_eq!(t.rows("procedure", "nope"), Err(Missing::Activity));
    }

    #[test]
    fn role_switch_true_only_for_enabled_job_and_platform() {
        for (platform, job, on) in [
            (false, false, false),
            (true, false, false),
            (false, true, false),
            (true, true, true),
        ] {
            let settings = if job { assistant_on() } else { BTreeMap::new() };
            let t = Toolsets::build(&bases(platform), [], settings);
            assert_eq!(
                t.settings.enabled(ROLE_ASSISTANT),
                on,
                "platform {platform} job {job}"
            );
        }
        // a role without a job switch follows its platform row
        let t = Toolsets::build(&bases(false), [], BTreeMap::new());
        assert!(t.settings.enabled("procedure"));
        assert!(t.settings.enabled_job("qualifier"));
        assert!(!t.settings.enabled("composer"));
    }

    #[test]
    fn budget_uses_role_setting_max_tool_calls() {
        let t = Toolsets::build(&bases(true), [], assistant_on());
        assert_eq!(t.settings.assistant_budget().max_tool_calls, 3);
        assert_eq!(t.settings.assistant_budget().turn, Budget::default().turn);
        let t = Toolsets::build(&bases(true), [], BTreeMap::new());
        assert_eq!(t.settings.assistant_budget().max_tool_calls, MAX_TOOL_CALLS);
        let off = RoleSetting {
            enabled: false,
            max_tool_calls: None,
            turn_budget_s: None,
            system_prompt: None,
            case_per_thread: false,
        };
        assert!(off.is_default());
        assert_eq!(
            serde_json::to_value(off).unwrap(),
            json!({"enabled": false, "max_tool_calls": null})
        );
    }
}
