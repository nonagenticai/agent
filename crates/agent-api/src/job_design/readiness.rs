//! B5's readiness contributors (T12): the `gates`, `procedures` and `toolsets` areas of the J3 board
//! (crate::readiness, registered in `readiness::registry()`), as the fixture `readinessOf` computes them
//! (`web/src/lib/api/areas/readiness.ts`). Every gap is go-live (a warning, never blocking). LOCAL STATE ONLY: the
//! design is read through the read API in `cx.design` with the routes' functions.
//!
//! - **Gates**: the job's effective questions; "reviewed" = the design holds any tenant gate row (a refinement, a
//!   setting, a tenant question, a generated option's overlay). Not reviewed → `gates_unreviewed` and the
//!   `dor_gates` step stays to do.
//! - **Procedures**: base names (template versions always run) plus the tenant's; a design whose procedures are all
//!   drafts → `procedure_unpublished`; a PUBLISHED tenant version whose steps leave its space's activity grant →
//!   `procedure_outside_grant` (one per version, the operations listed); a PUBLISHED version's tool requirement
//!   without a Production route → `procedure_tool_unrouted` (T7a, [`procedures::tool_unrouted`]).
//! - **Toolsets**: the roles at the design's pins that run (platform ∧ job) and the tenant's narrowings; no gap.
use std::collections::BTreeSet;
use std::sync::Arc;

use futures_util::future::BoxFuture;
use sqlx::PgConnection;

use super::context_space::{self, EffSpace};
use super::gates::library::{self, GateRows};
use super::gates::stats::Stats;
use super::procedures::{self, StoredVersion};
use super::types::VersionStatus;
use super::{grant_allows, op_label, toolset};
use crate::design;
use crate::error::ApiResult;
use crate::readiness::board::{area_shell, job_path, step};
use crate::readiness::{
    AreaKey, AreaStatus, Assessment, Blocks, Enforced, Fixer, Readiness, ReadinessContributor,
    ReadinessCx, ReadinessGap, ReadinessStatus,
};

/// The go-live warning of an unreviewed gate library.
pub const GATES_UNREVIEWED: &str = "gates_unreviewed";
/// The go-live warning of a job whose procedures are all drafts.
pub const PROCEDURE_UNPUBLISHED: &str = "procedure_unpublished";
/// The go-live warning of a PUBLISHED version with steps outside its activity's grant.
pub const PROCEDURE_OUTSIDE_GRANT: &str = "procedure_outside_grant";

/// Register B5's contributors (crate::readiness::registry).
pub fn register(r: &mut Readiness) {
    r.register(Arc::new(GatesReadiness));
    r.register(Arc::new(ProceduresReadiness));
    r.register(Arc::new(ToolsetsReadiness));
}

fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

#[allow(clippy::too_many_arguments)]
fn gap(
    area: AreaKey,
    key: String,
    subject: String,
    title: String,
    why: String,
    cause: &str,
    fix_label: &str,
    href: String,
) -> ReadinessGap {
    ReadinessGap {
        key,
        area,
        subject,
        title,
        why,
        cause: Some(cause.to_owned()),
        blocks: Blocks::GoLive,
        fixer: Fixer::TenantAdmin,
        fix_label: fix_label.to_owned(),
        href,
        enforced_from: Enforced::M4,
    }
}

// ---- gates ----------------------------------------------------------------------------------------------------------

/// The `gates` area.
pub struct GatesReadiness;

impl ReadinessContributor for GatesReadiness {
    fn area(&self) -> AreaKey {
        AreaKey::Gates
    }

    fn assess<'a>(
        &'a self,
        c: &'a mut PgConnection,
        cx: &'a ReadinessCx<'a>,
    ) -> BoxFuture<'a, ApiResult<Assessment>> {
        Box::pin(assess_gates(c, cx))
    }
}

async fn assess_gates(c: &mut PgConnection, cx: &ReadinessCx<'_>) -> ApiResult<Assessment> {
    let job = cx.job.key.as_str();
    let (bases, rows) =
        library::of_design(&mut *c, cx.reg, cx.compiler, cx.job, &cx.design).await?;
    let questions = library::effective(&bases, &rows, &Stats::default());
    let n = questions.len();
    let asked = questions.iter().filter(|q| q.enabled).count();
    let reviewed = rows != GateRows::default();
    let mut area = area_shell(AreaKey::Gates, job);
    area.template_part = plural(n, "gate question", "gate questions");
    area.ready = if reviewed { count(n) } else { 0 };
    area.total = count(n);
    let (status, detail) = if n == 0 {
        (
            AreaStatus::Todo,
            "The template has no gate questions".to_owned(),
        )
    } else if reviewed {
        (AreaStatus::Ok, format!("Reviewed; {asked} of {n} asked"))
    } else {
        (AreaStatus::Warn, "Not reviewed".to_owned())
    };
    area.status = status;
    let mut go_live = Vec::new();
    if status == AreaStatus::Warn {
        go_live.push(gap(
            AreaKey::Gates,
            "gates".to_owned(),
            "gate questions".to_owned(),
            "Gate questions not reviewed".to_owned(),
            "Not reviewed. Gates decide when the agent stops and asks; their wording and thresholds need your \
             review once."
                .to_owned(),
            GATES_UNREVIEWED,
            "Review gates",
            job_path(job, "/gates"),
        ));
    }
    area.summary = detail.clone();
    let status = if reviewed || n == 0 {
        ReadinessStatus::Done
    } else {
        ReadinessStatus::Todo
    };
    Ok(Assessment {
        area,
        blocking: Vec::new(),
        testing: Vec::new(),
        go_live,
        steps: vec![step(job, "dor_gates", status, detail)],
    })
}

// ---- procedures -----------------------------------------------------------------------------------------------------

/// The `procedures` area.
pub struct ProceduresReadiness;

impl ReadinessContributor for ProceduresReadiness {
    fn area(&self) -> AreaKey {
        AreaKey::Procedures
    }

    fn assess<'a>(
        &'a self,
        c: &'a mut PgConnection,
        cx: &'a ReadinessCx<'a>,
    ) -> BoxFuture<'a, ApiResult<Assessment>> {
        Box::pin(assess_procedures(c, cx))
    }
}

/// The step operations of version `v` that the activity grant of its space does not cover (routes' `grant_of`).
pub fn outside_grant(
    bases: &super::base::Bases,
    spaces: &[EffSpace],
    v: &StoredVersion,
) -> Vec<String> {
    let space = spaces.iter().find(|s| s.key == v.context_space);
    let grant = context_space::activity_ops(bases, space);
    let mut out: Vec<String> = Vec::new();
    for s in &v.steps {
        let label = op_label(&s.operation, s.variant.as_deref());
        if !out.contains(&label) && !grant_allows(&grant, &label) {
            out.push(label);
        }
    }
    out
}

async fn assess_procedures(c: &mut PgConnection, cx: &ReadinessCx<'_>) -> ApiResult<Assessment> {
    let job = cx.job.key.as_str();
    let (bases, spaces) =
        context_space::of_design(&mut *c, cx.reg, cx.compiler, cx.job, &cx.design).await?;
    let rows = design::values_of_kind(&mut *c, cx.reg, cx.job, &cx.design, "procedure").await?;
    let tenant = procedures::stored(&rows)?;
    let base_names: BTreeSet<String> = bases
        .procedures
        .keys()
        .map(|k| k.rsplit_once('@').map_or(k.as_str(), |(n, _)| n).to_owned())
        .collect();
    let mut names = base_names.clone();
    names.extend(tenant.keys().cloned());
    let mut published = base_names;
    let mut go_live = Vec::new();
    for (name, versions) in &tenant {
        for v in versions
            .iter()
            .filter(|v| v.status == VersionStatus::Published)
        {
            published.insert(name.clone());
            let outside = outside_grant(&bases, &spaces, v);
            if outside.is_empty() {
                continue;
            }
            let subject = format!("{name}@{}", v.version);
            go_live.push(gap(
                AreaKey::Procedures,
                format!("{PROCEDURE_OUTSIDE_GRANT}:{subject}"),
                subject.clone(),
                format!(
                    "{subject} runs {} outside its activity's grant",
                    outside.join(", ")
                ),
                format!(
                    "The runtime refuses a step whose operation the activity of {} does not grant: every case of \
                     {name} would stop there.",
                    if v.context_space.is_empty() {
                        "its context space"
                    } else {
                        v.context_space.as_str()
                    }
                ),
                PROCEDURE_OUTSIDE_GRANT,
                "Edit the procedure",
                job_path(job, "/procedures"),
            ));
        }
    }
    if !names.is_empty() && published.is_empty() {
        let drafts: Vec<&str> = names.iter().map(String::as_str).collect();
        go_live.push(gap(
            AreaKey::Procedures,
            "procedures".to_owned(),
            "procedures".to_owned(),
            "No procedure is published".to_owned(),
            format!(
                "{} {}. Drafts never run; without a published procedure the agent can only answer, not act.",
                drafts.join(", "),
                if drafts.len() == 1 {
                    "is a draft"
                } else {
                    "are drafts"
                }
            ),
            PROCEDURE_UNPUBLISHED,
            "Publish a procedure",
            job_path(job, "/procedures"),
        ));
    }
    go_live.extend(procedures::tool_unrouted(&mut *c, cx.reg, cx.job, &cx.design).await?);
    let (n, p) = (names.len(), published.len());
    let mut area = area_shell(AreaKey::Procedures, job);
    area.template_part = plural(n, "procedure", "procedures");
    area.ready = count(p);
    area.total = count(n);
    area.status = if n > 0 && p == 0 {
        AreaStatus::Gap
    } else if !go_live.is_empty() {
        AreaStatus::Warn
    } else if n > 0 {
        AreaStatus::Ok
    } else {
        AreaStatus::Todo
    };
    area.summary = if n > 0 {
        format!("{p} of {n} published; drafts do not run")
    } else {
        "No procedures yet".to_owned()
    };
    Ok(Assessment {
        area,
        blocking: Vec::new(),
        testing: Vec::new(),
        go_live,
        steps: Vec::new(),
    })
}

// ---- toolsets -------------------------------------------------------------------------------------------------------

/// The `toolsets` area.
pub struct ToolsetsReadiness;

impl ReadinessContributor for ToolsetsReadiness {
    fn area(&self) -> AreaKey {
        AreaKey::Toolsets
    }

    fn assess<'a>(
        &'a self,
        c: &'a mut PgConnection,
        cx: &'a ReadinessCx<'a>,
    ) -> BoxFuture<'a, ApiResult<Assessment>> {
        Box::pin(assess_toolsets(c, cx))
    }
}

async fn assess_toolsets(c: &mut PgConnection, cx: &ReadinessCx<'_>) -> ApiResult<Assessment> {
    let job = cx.job.key.as_str();
    let (_, t) = toolset::of_design(&mut *c, cx.reg, cx.compiler, cx.job, &cx.design).await?;
    let n = t.roles().len();
    let on = t
        .roles()
        .iter()
        .filter(|r| t.settings.enabled(&r.key))
        .count();
    let narrowed = t.narrowings().count();
    let mut area = area_shell(AreaKey::Toolsets, job);
    area.template_part = plural(n, "agent role with grants", "agent roles with grants");
    area.ready = count(on);
    area.total = count(n);
    area.status = if on > 0 {
        AreaStatus::Ok
    } else {
        AreaStatus::Todo
    };
    area.summary = format!(
        "{on} of {n} roles on · {} by you",
        plural(narrowed, "narrowing", "narrowings")
    );
    Ok(Assessment {
        area,
        blocking: Vec::new(),
        testing: Vec::new(),
        go_live: Vec::new(),
        steps: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job_design::base::{ActivityOp, BaseActivity, Bases};
    use serde_json::json;

    fn version(space: &str, ops: &[(&str, Option<&str>)]) -> StoredVersion {
        let steps: Vec<serde_json::Value> = ops
            .iter()
            .enumerate()
            .map(|(i, (op, variant))| {
                json!({"step_number": i + 1, "phase": 0, "name": format!("s{i}"), "operation": op,
                       "variant": variant, "modality": "do", "side_effect": "R", "identity_level": "2",
                       "confirm_back": false, "gate": false, "args": {}})
            })
            .collect();
        serde_json::from_value(
            json!({"version": 2, "status": "PUBLISHED", "context_space": space,
                                       "steps": steps}),
        )
        .unwrap()
    }

    #[test]
    fn steps_outside_the_activity_grant_are_listed_once() {
        let mut b = Bases::default();
        b.context_spaces.insert(
            "retail:order-support:returns".into(),
            json!({"label": "Returns", "activity": "support:start_return"})
                .as_object()
                .cloned()
                .unwrap(),
        );
        b.activities.push(BaseActivity {
            key: "support:start_return".into(),
            item: serde_json::Map::new(),
            ops: vec![
                ActivityOp {
                    op: "order.read".into(),
                    variant: None,
                    required: true,
                },
                ActivityOp {
                    op: "case.create".into(),
                    variant: None,
                    required: false,
                },
            ],
        });
        let spaces = context_space::merge(&b, &std::collections::BTreeMap::new());
        let v = version(
            "retail:order-support:returns",
            &[
                ("order.read", None),
                ("case.create", Some("return")),
                ("payment.refund", None),
                ("payment.refund", None),
            ],
        );
        assert_eq!(outside_grant(&b, &spaces, &v), ["payment.refund"]);
        // a space no activity serves: every step is outside
        let w = version("retail:faq:none", &[("order.read", None)]);
        assert_eq!(outside_grant(&b, &spaces, &w), ["order.read"]);
    }
}
