//! B5's [`RoleSwitch`] (T11a, F-40): the `assistant` role runs for a job when the job's `role_setting` switches it on
//! AND the platform `agent_role` row at the design's pins is enabled. Read from the case's design
//! (`d.job_design.roles`): Live in Production, Draft in Test. The setting is per job, not per activity (the
//! activity's toolset decides what the assistant may call).
use super::toolset::ROLE_ASSISTANT;
use crate::runtime::assistant::RoleSwitch;
use crate::runtime::design::RuntimeDesign;

#[derive(Clone, Copy, Debug, Default)]
pub struct JobRoleSwitch;

impl RoleSwitch for JobRoleSwitch {
    fn assistant_enabled(&self, d: &RuntimeDesign, _activity: &str) -> bool {
        d.job_design.roles.enabled(ROLE_ASSISTANT)
    }
}
