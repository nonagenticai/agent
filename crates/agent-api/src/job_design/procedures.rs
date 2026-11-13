//! B5 T7a (F-15, F-20): the job's tenant procedure versions as the runtime and readiness read them.
//!
//! - [`stored`] decodes the `procedure` places of a design (B2 read API, `values_of_kind`): the versions with their
//!   step bodies (pre/postconditions, `performed_by`, `max_per_case`), `success_criteria`, `tool_requirements`. A
//!   value written before T7a (migration 0147) lacks the version-level fields and its steps lack the step-level ones:
//!   they read as empty / absent.
//! - [`runtime_procedure`] is a PUBLISHED tenant version as B4's [`TemplateProcedure`] (what T12's
//!   `RuntimeDesign::read` puts in `RuntimeDesign.procedures`, shadowing the base `name@N`); [`published`] lists them
//!   for one design.
//! - [`tool_unrouted`]: a PUBLISHED version's tool requirement that does not run in Production is a go-live warning
//!   `procedure_tool_unrouted` (area `procedures`; B4's orchestrator would hand such a case over before its first
//!   step). T12's procedures readiness contributor reports it.
use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::Value;
use sqlx::PgConnection;

use crate::connectors::opkey::parse_op_key;
use crate::connectors::readiness::cell_of;
use crate::connectors::resolve::RouteData;
use crate::connectors::{places, routes_store};
use crate::design::{self, DesignOf, DesignRow, JobRef, Registry};
use crate::error::ApiResult;
use crate::job_design::types::{Condition, ProcedureStep, VersionStatus};
use crate::readiness::board::job_path;
use crate::readiness::{AreaKey, Blocks, Enforced, Fixer, ReadinessGap};
use crate::runtime::spaces::TemplateProcedure;

/// The go-live warning code of a tool requirement without a Production route.
pub const TOOL_UNROUTED: &str = "procedure_tool_unrouted";

/// One tenant version as a `procedure` place stores it (`job_design::targets::procedures`).
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct StoredVersion {
    pub version: u32,
    pub status: VersionStatus,
    #[serde(default)]
    pub context_space: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub steps: Vec<ProcedureStep>,
    #[serde(default)]
    pub success_criteria: Vec<Condition>,
    #[serde(default)]
    pub tool_requirements: Vec<String>,
    #[serde(default)]
    pub pddl_domain: Option<String>,
}

/// The tenant versions of `rows` (the `procedure` places of one design), by name, oldest first.
pub fn stored(rows: &[DesignRow]) -> ApiResult<BTreeMap<String, Vec<StoredVersion>>> {
    let mut out: BTreeMap<String, Vec<StoredVersion>> = BTreeMap::new();
    for row in rows.iter().filter(|r| r.kind == "procedure") {
        let Some(name) = row.r#ref.get("name").and_then(Value::as_str) else {
            continue;
        };
        let Some(v) = row.snap.as_ref().filter(|v| !v.is_null()) else {
            continue;
        };
        // A value of another shape (written straight through the design target, or by an older shape) is skipped,
        // never a 500: the readiness gate on approve/apply and `RuntimeDesign::read` both read it.
        let mut versions: Vec<StoredVersion> = match serde_json::from_value(v.clone()) {
            Ok(vs) => vs,
            Err(e) => {
                tracing::warn!(procedure = name, error = %e, "procedure value unreadable; skipped");
                continue;
            }
        };
        versions.sort_by_key(|x| x.version);
        out.entry(name.to_owned()).or_default().extend(versions);
    }
    Ok(out)
}

/// A tenant version as B4's procedure (label = its description, `steps` = the step count).
pub fn runtime_procedure(name: &str, v: &StoredVersion) -> TemplateProcedure {
    TemplateProcedure {
        name: name.to_owned(),
        version: v.version,
        label: v.description.clone(),
        uses_slots: Vec::new(),
        steps: u32::try_from(v.steps.len()).unwrap_or(u32::MAX),
        step_defs: v.steps.clone(),
        success_criteria: v.success_criteria.clone(),
        tool_requirements: v.tool_requirements.clone(),
        context_space: Some(v.context_space.clone()).filter(|k| !k.is_empty()),
        // set by `runtime::effective_procedures` from the base version it shadows
        builtin_version: None,
        tenant: true,
    }
}

/// The PUBLISHED tenant version of each name in `rows` (at most one per name, 0142's partial UNIQUE index).
pub fn published_of(rows: &[DesignRow]) -> ApiResult<Vec<TemplateProcedure>> {
    Ok(stored(rows)?
        .iter()
        .flat_map(|(name, vs)| {
            vs.iter()
                .filter(|v| v.status == VersionStatus::Published)
                .map(move |v| runtime_procedure(name, v))
        })
        .collect())
}

/// The PUBLISHED tenant versions of design `of`, as B4's procedures.
pub async fn published(
    c: &mut PgConnection,
    reg: &Registry,
    job: &JobRef,
    of: &DesignOf,
) -> ApiResult<Vec<TemplateProcedure>> {
    let rows = design::values_of_kind(&mut *c, reg, job, of, "procedure").await?;
    published_of(&rows)
}

/// The go-live warnings of `procs`: one per tool requirement `runs` answers `Some(why)` for (it does not run in
/// Production).
pub fn tool_unrouted_gaps(
    job_key: &str,
    procs: &[TemplateProcedure],
    runs: impl Fn(&str) -> Option<String>,
) -> Vec<ReadinessGap> {
    let mut out = Vec::new();
    for p in procs {
        for op in &p.tool_requirements {
            let Some(why) = runs(op) else {
                continue;
            };
            out.push(ReadinessGap {
                key: format!("{TOOL_UNROUTED}:{}@{}:{op}", p.name, p.version),
                area: AreaKey::Procedures,
                subject: format!("{}@{}", p.name, p.version),
                title: format!(
                    "{}@{} needs {op}, which has no Production route",
                    p.name, p.version
                ),
                why: format!(
                    "Every case of {} would be handed over before its first step: {why}",
                    p.name
                ),
                cause: Some(TOOL_UNROUTED.to_owned()),
                blocks: Blocks::GoLive,
                fixer: Fixer::TenantAdmin,
                fix_label: format!("Route {op}"),
                href: job_path(job_key, "/operations"),
                enforced_from: Enforced::M4,
            });
        }
    }
    out
}

/// [`tool_unrouted_gaps`] of design `of`: its PUBLISHED tenant versions against its own routes (B7 `cell_of`,
/// env `prod`, as the routes contributor reads them).
pub async fn tool_unrouted(
    c: &mut PgConnection,
    reg: &Registry,
    job: &JobRef,
    of: &DesignOf,
) -> ApiResult<Vec<ReadinessGap>> {
    let procs = published(&mut *c, reg, job, of).await?;
    if procs.iter().all(|p| p.tool_requirements.is_empty()) {
        return Ok(Vec::new());
    }
    let routes = places::design_routes(&mut *c, reg, job, of).await?;
    let row = routes_store::job_row(&mut *c, job.id).await?;
    let base = routes_store::view_of(
        &mut *c,
        row,
        routes_store::ExpectedCatalogue::default(),
        Vec::new(),
    )
    .await?
    .data;
    let data = RouteData { routes, ..base };
    Ok(tool_unrouted_gaps(&job.key, &procs, |op| {
        let cell = cell_of(&data, &parse_op_key(op), "prod");
        (!cell.runs).then_some(cell.why)
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(name: &str, snap: Value) -> DesignRow {
        let mut r = design::DraftRef::new();
        r.insert("name".into(), json!(name));
        DesignRow {
            key: format!("procedure:{name}"),
            kind: "procedure".into(),
            r#ref: r,
            snap: Some(snap),
        }
    }

    #[test]
    fn values_before_t7a_read_with_defaults() {
        let old = row(
            "refund",
            json!([{"version": 2, "status": "PUBLISHED", "context_space": "x", "description": "Refund",
                    "reviewers": [], "released_in": null,
                    "steps": [{"step_number": 1, "phase": 0, "name": "order", "operation": "order.read",
                               "variant": null, "modality": "observe", "side_effect": "R", "identity_level": "2",
                               "confirm_back": false, "gate": false, "args": {}}]}]),
        );
        let ps = published_of(&[old]).unwrap();
        assert_eq!(ps.len(), 1);
        let p = &ps[0];
        assert_eq!((p.name.as_str(), p.version, p.steps), ("refund", 2, 1));
        assert_eq!(
            p.context_space.as_deref(),
            Some("x"),
            "the space it serves reaches the runtime"
        );
        assert!(p.success_criteria.is_empty() && p.tool_requirements.is_empty());
        let s = &p.step_defs[0];
        assert!(s.preconditions.is_empty() && s.postconditions.is_empty());
        assert_eq!((s.performed_by.as_deref(), s.max_per_case), (None, None));
    }

    #[test]
    fn only_published_versions_reach_the_runtime() {
        let r = row(
            "refund",
            json!([
                {"version": 1, "status": "ARCHIVED", "steps": []},
                {"version": 2, "status": "PUBLISHED", "steps": [], "tool_requirements": ["order.cancel"],
                 "success_criteria": [{"id": "done", "fact": "case.activity", "op": "exists"}]},
                {"version": 3, "status": "DRAFT", "steps": []},
            ]),
        );
        let ps = published_of(&[r]).unwrap();
        assert_eq!(ps.len(), 1);
        assert_eq!(ps[0].version, 2);
        assert_eq!(ps[0].context_space, None, "no stored space reads as none");
        assert_eq!(ps[0].tool_requirements, ["order.cancel"]);
        assert_eq!(ps[0].success_criteria[0].id, "done");
    }

    #[test]
    fn unrouted_tool_requirements_are_go_live_warnings() {
        let mut p = runtime_procedure(
            "refund",
            &StoredVersion {
                version: 4,
                status: VersionStatus::Published,
                context_space: String::new(),
                description: String::new(),
                steps: Vec::new(),
                success_criteria: Vec::new(),
                tool_requirements: vec!["order.read".into(), "order.cancel".into()],
                pddl_domain: None,
            },
        );
        let gaps = tool_unrouted_gaps("care", std::slice::from_ref(&p), |op| {
            (op == "order.cancel").then(|| "no route".to_owned())
        });
        assert_eq!(gaps.len(), 1);
        let g = &gaps[0];
        assert_eq!(g.key, "procedure_tool_unrouted:refund@4:order.cancel");
        assert_eq!((g.area, g.blocks), (AreaKey::Procedures, Blocks::GoLive));
        assert_eq!(g.cause.as_deref(), Some(TOOL_UNROUTED));
        assert_eq!(g.href, "/admin/jobs/care/operations");
        p.tool_requirements.clear();
        assert!(tool_unrouted_gaps("care", &[p], |_| Some("x".into())).is_empty());
    }
}
