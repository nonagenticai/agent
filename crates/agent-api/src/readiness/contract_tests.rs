//! The `tests` area of the J3 board and the `dor_tests` definition-of-ready line (B2 §2.6 "tests: B7 (contract
//! tests), blocks apply: no"; fixture `readinessOf` `dor_tests` / `buildBoard` area `tests` and its go-live gap).
//!
//! It READS the B7 contract results (`connector_contract_result`, one latest row per instance × op, through
//! crate::connectors::routes_store) and never runs a test. Its subjects are the bundle's required Production routes
//! (`requires.routes`) that land on a connector instance in the design checked; built-in, `agent_cases`, deep-link
//! and unrouted operations have nothing to contract-test (the routes area owns unrouted ones). Each subject's
//! B7 cell (crate::connectors::readiness::cell_of) gives its latest result:
//!
//! - `ready` (green, or a platform-tested instance) → proven;
//! - `red`, or an instance that fails every call / no longer exists (`failing`) → failing → `blocked`;
//! - `needs_ok` / `unverified` (a prod write not test-written) → attention → `current` (fixture "1 write unverified
//!   (payment.create) — needs an explicit OK for prod writes");
//! - `untested` (never run) or a stub ("stubs never count as tested") → pending → `todo`.
//!
//! The line is the worst of them: failing, else attention, else pending, else `done` "All green". A job with required
//! routes but none on an instance is `todo` "No contract tests — nothing connected" when one is unrouted, else
//! `done`. A job that requires no route keeps the area and the line "Not checked yet". Any non-`done` line is a
//! go-live gap (warning `contract_tests_pending`), never a blocking item.
use std::sync::Arc;

use futures_util::future::BoxFuture;
use sqlx::PgConnection;

use super::board::{area_shell, job_path, split_route, step};
use super::{
    AreaKey, AreaStatus, Assessment, Blocks, Enforced, Fixer, Readiness, ReadinessContributor,
    ReadinessCx, ReadinessGap, ReadinessStatus,
};
use crate::connectors::opkey::parse_op_key;
use crate::connectors::places;
use crate::connectors::readiness::{RouteCell, cell_of};
use crate::connectors::resolve::RouteData;
use crate::connectors::routes_store::{self, ExpectedCatalogue};
use crate::error::ApiResult;

/// Register the contract-tests contributor (one line in crate::readiness::registry).
pub fn register(r: &mut Readiness) {
    r.register(Arc::new(ContractTestsReadiness));
}

/// The `tests` area of the J3 board and the `dor_tests` line.
pub struct ContractTestsReadiness;

impl ReadinessContributor for ContractTestsReadiness {
    fn area(&self) -> AreaKey {
        AreaKey::Tests
    }

    fn assess<'a>(
        &'a self,
        c: &'a mut PgConnection,
        cx: &'a ReadinessCx<'a>,
    ) -> BoxFuture<'a, ApiResult<Assessment>> {
        Box::pin(assess(c, cx))
    }
}

/// What the latest contract result of one subject says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Outcome {
    Green,
    Pending,
    Attention,
    Failing,
}

/// The outcome of a required Production route's cell; `None` = no instance behind it (nothing to contract-test).
pub fn outcome_of(cell: &RouteCell) -> Option<Outcome> {
    cell.instance.as_ref().map(|_| match cell.state.as_str() {
        "ready" => Outcome::Green,
        "red" | "failing" => Outcome::Failing,
        "needs_ok" | "unverified" => Outcome::Attention,
        _ => Outcome::Pending,
    })
}

/// One subject: `op@env`, its instance and outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subject {
    pub route: String,
    pub instance: String,
    pub outcome: Outcome,
}

/// The `dor_tests` status and detail over `subjects`; `unrouted` = some required route has no target at all.
pub fn line(subjects: &[Subject], unrouted: bool) -> (ReadinessStatus, String) {
    let named = |v: Outcome| {
        subjects
            .iter()
            .filter(|s| s.outcome == v)
            .map(|s| format!("{} on {}", split_route(&s.route).0, s.instance))
            .collect::<Vec<_>>()
    };
    let Some(worst) = subjects.iter().map(|s| s.outcome).max() else {
        return if unrouted {
            (
                ReadinessStatus::Todo,
                "No contract tests \u{2014} nothing connected".to_owned(),
            )
        } else {
            (
                ReadinessStatus::Done,
                "No required operation reaches a connected system: nothing to test".to_owned(),
            )
        };
    };
    match worst {
        Outcome::Failing => {
            let xs = named(Outcome::Failing);
            (
                ReadinessStatus::Blocked,
                format!("{} failing: {}", xs.len(), xs.join(", ")),
            )
        }
        Outcome::Attention => {
            let xs = named(Outcome::Attention);
            (
                ReadinessStatus::Current,
                format!(
                    "{} write unverified ({}) \u{2014} needs an explicit OK for prod writes",
                    xs.len(),
                    xs.join(", ")
                ),
            )
        }
        Outcome::Pending => {
            let xs = named(Outcome::Pending);
            (
                ReadinessStatus::Todo,
                format!("{} not run yet: {}", xs.len(), xs.join(", ")),
            )
        }
        Outcome::Green => (ReadinessStatus::Done, "All green".to_owned()),
    }
}

fn area_status(s: ReadinessStatus) -> AreaStatus {
    match s {
        ReadinessStatus::Done => AreaStatus::Ok,
        ReadinessStatus::Current | ReadinessStatus::Placeholder => AreaStatus::Warn,
        ReadinessStatus::Todo => AreaStatus::Todo,
        ReadinessStatus::Missing | ReadinessStatus::Blocked => AreaStatus::Gap,
    }
}

async fn assess(c: &mut PgConnection, cx: &ReadinessCx<'_>) -> ApiResult<Assessment> {
    let key = cx.job.key.as_str();
    let mut area = area_shell(AreaKey::Tests, key);
    if cx.requires.routes.is_empty() {
        return Ok(Assessment {
            area,
            blocking: Vec::new(),
            testing: Vec::new(),
            go_live: Vec::new(),
            steps: Vec::new(),
        });
    }
    // the routes of the design checked, over the tenant's instances and their latest contract results (live, D69)
    let routes = places::design_routes(&mut *c, cx.reg, cx.job, &cx.design).await?;
    let row = routes_store::job_row(&mut *c, cx.job.id).await?;
    let base = routes_store::view_of(&mut *c, row, ExpectedCatalogue::default(), routes)
        .await?
        .data;
    let (subjects, unrouted) = subjects_of(&base, &cx.requires.routes);
    let (status, detail) = line(&subjects, unrouted);
    let green = subjects
        .iter()
        .filter(|s| s.outcome == Outcome::Green)
        .count();
    area.ready = u32::try_from(green).unwrap_or(u32::MAX);
    area.total = u32::try_from(subjects.len()).unwrap_or(u32::MAX);
    area.status = area_status(status);
    area.summary = detail.clone();
    let href = job_path(key, "/operations");
    let go_live = if status == ReadinessStatus::Done {
        Vec::new()
    } else {
        vec![ReadinessGap {
            key: "tests".to_owned(),
            area: AreaKey::Tests,
            subject: "contract tests".to_owned(),
            title: "Not every operation is proven against its system".to_owned(),
            why: format!("{detail}. Stubs and unverified writes never count as tested."),
            cause: None,
            blocks: Blocks::GoLive,
            fixer: Fixer::TenantAdmin,
            fix_label: "Review contract tests".to_owned(),
            href,
            enforced_from: Enforced::M4,
        }]
    };
    Ok(Assessment {
        area,
        blocking: Vec::new(),
        testing: Vec::new(),
        go_live,
        steps: vec![step(key, "dor_tests", status, detail)],
    })
}

/// The subjects of `required` (`op@env` keys) over `d`, and whether any required route has no target.
pub fn subjects_of(d: &RouteData, required: &[String]) -> (Vec<Subject>, bool) {
    let mut out = Vec::new();
    let mut unrouted = false;
    for r in required {
        let (op, env) = split_route(r);
        let op = parse_op_key(op);
        if env.is_empty() || op.object.is_empty() {
            continue;
        }
        let cell = cell_of(d, &op, env);
        if cell.state == "unrouted" {
            unrouted = true;
        }
        if let (Some(v), Some(instance)) = (outcome_of(&cell), cell.instance.clone()) {
            out.push(Subject {
                route: r.clone(),
                instance,
                outcome: v,
            });
        }
    }
    (out, unrouted)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(route: &str, v: Outcome) -> Subject {
        Subject {
            route: route.to_owned(),
            instance: "shop-prod".to_owned(),
            outcome: v,
        }
    }

    #[test]
    fn the_worst_subject_decides() {
        assert_eq!(line(&[], true).0, ReadinessStatus::Todo);
        assert_eq!(line(&[], false).0, ReadinessStatus::Done);
        let green = s("case.read@prod", Outcome::Green);
        assert_eq!(
            line(std::slice::from_ref(&green), false),
            (ReadinessStatus::Done, "All green".to_owned())
        );
        let pending = s("order.read@prod", Outcome::Pending);
        assert_eq!(
            line(&[green.clone(), pending.clone()], false),
            (
                ReadinessStatus::Todo,
                "1 not run yet: order.read on shop-prod".to_owned()
            )
        );
        let attention = s("payment.create@prod", Outcome::Attention);
        assert_eq!(
            line(&[pending.clone(), attention.clone()], false).0,
            ReadinessStatus::Current
        );
        let failing = s("case.create{return}@prod", Outcome::Failing);
        assert_eq!(
            line(&[green, pending, attention, failing], false),
            (
                ReadinessStatus::Blocked,
                "1 failing: case.create{return} on shop-prod".to_owned()
            )
        );
    }
}
