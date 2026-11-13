//! B7 T13 (D67): route cells and routing readiness. The pure part is the Rust port of TS `cellOf` / `cellsOf` /
//! `opReadinessOf` (`web/src/lib/api/areas/readiness.ts`); [`cells`] and [`routing`] read a job's routes from the database (crate::connectors::routes_store).
//!
//! [`RoutesReadiness`] is B2's `ReadinessContributor` for `AreaKey::Routes` (D67, registered in
//! crate::readiness::registry): the bundle's required Production routes (`requires.routes`) that do not run in the
//! design checked are blocking (`route_not_ready`); its required Test-env routes (`requires.test_routes`) that do
//! not run in the draft are testing gaps (fixture `missingFor` / `testEnvFor` / `buildBoard` "routes").
//!
//! Owner ruling R5 (2026-10-21): routing is a HARD publish blocker. A job whose bundle uses at least one Production
//! operation that does not run there is refused even when it has no route rows at all (Production would answer every
//! such call `no_route`); a job that uses no operation (`requires.routes` empty) publishes.
use std::collections::BTreeMap;
use std::sync::Arc;

use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use sqlx::PgConnection;
use uuid::Uuid;

use super::model::OpRef;
use super::opkey::{op_key, parse_op_key};
use super::places;
use super::resolve::{OperationRoute, RouteData, RouteTarget, resolve_route};
use super::routes_store::{self, DesignOf, ExpectedCatalogue};
use crate::design::{self, Registry};
use crate::error::ApiResult;
use crate::job_design::op_label;
use crate::job_design::runtime as job_runtime;
use crate::readiness::board::{area_shell, job_path, split_route, step};
use crate::readiness::{
    AreaKey, AreaStatus, Assessment, MissingItem, Readiness, ReadinessContributor, ReadinessCx,
    ReadinessStatus,
};
use crate::release::compile::TemplateCompiler;
use crate::runtime::routes::{DefaultRoutes, ResolvedTarget, RouteQuery};
use crate::runtime::runner::ops::{route_for, target_of};
use crate::runtime::scope::Env;
use crate::runtime::spaces::TemplateProcedure;

/// TS `RouteCell`: one op × env of the route matrix, as the backend resolves it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteCell {
    /// `ready` | `fallback` | `unverified` | `needs_ok` | `untested` | `red` | `failing` | `stub` | `unrouted` |
    /// `not_offered`.
    pub state: String,
    /// The op actually runs for a customer (ready, fallback, or a confirm-back write).
    pub runs: bool,
    pub target: Option<RouteTarget>,
    pub inherited: bool,
    pub suggested: Option<RouteTarget>,
    /// Instance id the route lands on (null for built-in / fallbacks / unrouted).
    pub instance: Option<String>,
    pub why: String,
}

/// States in which the op actually runs for a customer.
const RUNS: [&str; 3] = ["ready", "fallback", "unverified"];

/// TS `cellOf`: can `op` run on `env`.
pub fn cell_of(d: &RouteData, op: &OpRef, env: &str) -> RouteCell {
    let r = resolve_route(d, op, env);
    let target = r.route.as_ref().map(|x| x.target.clone());
    let cell = |state: &str, why: String, instance: Option<String>| RouteCell {
        state: state.to_owned(),
        runs: RUNS.contains(&state),
        target: target.clone(),
        inherited: r.inherited,
        suggested: r.suggested.clone(),
        instance,
        why,
    };
    let Some(t) = target.as_ref() else {
        return cell(
            "unrouted",
            "No route \u{2014} the agent cannot do this here.".to_owned(),
            None,
        );
    };
    let id = match t {
        RouteTarget::NotOffered => {
            return cell(
                "not_offered",
                "Optional and switched off: the agent says it cannot do this.".to_owned(),
                None,
            );
        }
        RouteTarget::Builtin => {
            return cell(
                "ready",
                "Runs inside the agent; no system needed.".to_owned(),
                None,
            );
        }
        RouteTarget::AgentCases => {
            return cell(
                "fallback",
                "Becomes a case in the staff queue; a person finishes it.".to_owned(),
                None,
            );
        }
        RouteTarget::DeepLink { .. } => {
            return cell(
                "fallback",
                "The agent sends the customer a link to your site.".to_owned(),
                None,
            );
        }
        RouteTarget::Instance { instance_id, .. } => instance_id.clone(),
    };
    let Some(inst) = d.instances.iter().find(|i| i.id == id) else {
        return cell("failing", format!("{id} no longer exists."), Some(id));
    };
    if inst.stub {
        return cell(
            "stub",
            format!(
                "{} replays fixtures \u{2014} stubs never count as ready.",
                inst.id
            ),
            Some(id),
        );
    }
    if inst.status == "auth_failed" || inst.status == "invalid_target" {
        return cell(
            "failing",
            format!("{} fails every call ({}).", inst.id, inst.status),
            Some(id),
        );
    }
    if inst.platform {
        return cell(
            "ready",
            format!("{} is provisioned and tested by the platform.", inst.id),
            Some(id),
        );
    }
    let served = if r.inherited {
        op_key(&OpRef {
            object: op.object.clone(),
            verb: op.verb.clone(),
            variant: "*".to_owned(),
        })
    } else {
        op_key(op)
    };
    let res = d
        .contract
        .as_ref()
        .and_then(|c| c.get(&inst.id))
        .and_then(|rs| rs.iter().find(|c| c.op == served));
    let green = d.tested.get(&inst.id).is_some_and(|t| t.contains(&served));
    if green || res.is_some_and(|c| c.outcome == "green") {
        return cell(
            "ready",
            format!("Contract test green on {}.", inst.id),
            Some(id),
        );
    }
    match res.map(|c| c.outcome.as_str()) {
        Some("red") => {
            let note = res.map(|c| c.note.as_str()).unwrap_or_default();
            let tail = if note.is_empty() {
                String::new()
            } else {
                format!(": {note}")
            };
            cell(
                "red",
                format!("Contract test red on {}{tail}.", inst.id),
                Some(id),
            )
        }
        Some("needs_ok") => cell(
            "needs_ok",
            format!(
                "A prod write is only tested after you allow one tagged test write on {}.",
                inst.id
            ),
            Some(id),
        ),
        Some("write_unverified") => cell(
            "unverified",
            "Not test-written on prod: each call is confirmed by the customer and checked by a read-back."
                .to_owned(),
            Some(id),
        ),
        _ => cell(
            "untested",
            format!(
                "No contract test has run for {served} on {} yet.",
                inst.id
            ),
            Some(id),
        ),
    }
}

/// TS `cellsOf`: every op × env of the job, keyed `object.verb{variant}@env`.
pub fn cells_of(d: &RouteData) -> BTreeMap<String, RouteCell> {
    let mut out = BTreeMap::new();
    for o in &d.ops {
        let op = o.op_ref();
        for env in ["prod", "test"] {
            out.insert(format!("{}@{env}", op_key(&op)), cell_of(d, &op, env));
        }
    }
    out
}

/// TS `opReadinessOf`: can one system serve one of its template ops (T11).
pub fn op_readiness_of(outcome: &str, routed: bool, stub: bool, platform: bool) -> &'static str {
    if !routed {
        return "unrouted";
    }
    if stub {
        return "stub";
    }
    if platform {
        return "ready";
    }
    match outcome {
        "green" => "ready",
        "red" => "blocked",
        "needs_ok" => "needs_ok",
        "write_unverified" => "unverified",
        _ => "untested",
    }
}

/// One item a publish refuses on (B2 `ReadinessItem`, code `route_not_ready`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BlockingItem {
    pub code: &'static str,
    /// `route:<op>@prod`.
    pub subject: String,
    /// The cell's `why`.
    pub message: String,
    pub href: String,
}

/// D67 routing readiness of one job: every required op has a Production cell that runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RoutingReadiness {
    pub routing_ready: bool,
    pub blocking: Vec<BlockingItem>,
}

/// Pure: [`RoutingReadiness`] over `d` and its cells (`job` names the J6 page in `href`).
pub fn routing_ready(
    job: &str,
    d: &RouteData,
    cells: &BTreeMap<String, RouteCell>,
) -> RoutingReadiness {
    let mut blocking = Vec::new();
    for o in d.ops.iter().filter(|o| o.required) {
        let key = op_key(&o.op_ref());
        let cell = cells.get(&format!("{key}@prod"));
        if cell.is_some_and(|c| c.runs) {
            continue;
        }
        blocking.push(BlockingItem {
            code: "route_not_ready",
            subject: format!("route:{key}@prod"),
            message: cell.map_or_else(
                || "No route \u{2014} the agent cannot do this here.".to_owned(),
                |c| c.why.clone(),
            ),
            href: format!("/admin/jobs/{}/operations", urlencoding_job(job)),
        });
    }
    RoutingReadiness {
        routing_ready: blocking.is_empty(),
        blocking,
    }
}

/// Job keys are `^[a-z][a-z0-9_]{1,62}$` (0033): nothing to encode; anything else is percent-encoded bytewise.
fn urlencoding_job(job: &str) -> String {
    job.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.' || b == b'~' {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// D67 cells of `job` read `of` the draft (working rows: what the test env runs) or Production (its current
/// release; never released → no cells: Production never reads draft rows).
pub async fn cells(
    conn: &mut PgConnection,
    reg: &Registry,
    job: Uuid,
    of: DesignOf,
) -> ApiResult<BTreeMap<String, RouteCell>> {
    let view = routes_store::route_data(conn, reg, job, of).await?;
    Ok(cells_of(&view.data))
}

/// D67 routing readiness of `job` `of` the draft or Production. A job never released is never routing-ready in
/// Production (fail closed) and lists nothing.
pub async fn routing(
    conn: &mut PgConnection,
    reg: &Registry,
    job: Uuid,
    job_key: &str,
    of: DesignOf,
) -> ApiResult<RoutingReadiness> {
    if of == DesignOf::Live && places::current_release(&mut *conn, job).await?.is_none() {
        return Ok(RoutingReadiness {
            routing_ready: false,
            blocking: Vec::new(),
        });
    }
    let view = routes_store::route_data(conn, reg, job, of).await?;
    let c = cells_of(&view.data);
    Ok(routing_ready(job_key, &view.data, &c))
}

// ---- B2 readiness contributor (D67) ---------------------------------------------------------------------------------

/// Register the routes contributor (one line in crate::readiness::registry).
pub fn register(r: &mut Readiness) {
    r.register(Arc::new(RoutesReadiness));
}

/// The `routes` area of the J3 board and the publish gate.
pub struct RoutesReadiness;

impl ReadinessContributor for RoutesReadiness {
    fn area(&self) -> AreaKey {
        AreaKey::Routes
    }

    fn assess<'a>(
        &'a self,
        c: &'a mut PgConnection,
        cx: &'a ReadinessCx<'a>,
    ) -> BoxFuture<'a, ApiResult<Assessment>> {
        Box::pin(assess(c, cx))
    }
}

/// Fixture `routeReady`: the cell of `key` (`object.verb{variant}@env`) over `base` with `routes`; `None` when the
/// key does not parse (it never runs).
fn route_cell(base: &RouteData, routes: &[OperationRoute], key: &str) -> Option<RouteCell> {
    let (op, env) = split_route(key);
    if env.is_empty() {
        return None;
    }
    let op = parse_op_key(op);
    if op.object.is_empty() {
        return None;
    }
    let data = RouteData {
        routes: routes.to_vec(),
        ..base.clone()
    };
    Some(cell_of(&data, &op, env))
}

/// Does the route of `key` run over `base` with `routes`.
fn route_runs(base: &RouteData, routes: &[OperationRoute], key: &str) -> bool {
    route_cell(base, routes, key).is_some_and(|c| c.runs)
}

/// R5: the required Production routes of `requires` that do not run over `base` with `routes` (the design checked),
/// whether or not the job has any route row. Each names its operation; an op with no route at all says calls would
/// fail `no_route`.
fn unrouted_prod(
    base: &RouteData,
    routes: &[OperationRoute],
    required: &[String],
    href: &str,
) -> Vec<MissingItem> {
    required
        .iter()
        .filter_map(|r| {
            let cell = route_cell(base, routes, r);
            if cell.as_ref().is_some_and(|c| c.runs) {
                return None;
            }
            let (op, env) = split_route(r);
            let detail = match cell {
                Some(c) if c.state != "unrouted" => {
                    format!("Required operation {op} has no tested route on {env}: {}", c.why)
                }
                _ => format!(
                    "Required operation {op} has no route on {env}: every call would fail (no_route). Route it before publishing"
                ),
            };
            Some(MissingItem {
                dependency: "routing".to_string(),
                slot: r.clone(),
                detail,
                href: href.to_string(),
            })
        })
        .collect()
}

/// Prefix of the `detail` of a routing item that a procedure causes: [`crate::readiness::board`] shows such a detail
/// to the admin in full (it names the operation and the procedures).
pub const PROCEDURE_DETAIL: &str = "Needed by ";

/// Every operation the effective `procs` need to run, as `object.verb{variant}` labels, with the names of the
/// procedures that need it: their `tool_requirements` (the orchestrator hands a case over before its first step when
/// one is not offered) and their steps' operations (the runner answers `no_route` there).
pub fn procedure_ops(procs: &[TemplateProcedure]) -> BTreeMap<String, Vec<String>> {
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for p in procs {
        let steps = p
            .step_defs
            .iter()
            .map(|s| op_label(&s.operation, s.variant.as_deref()));
        for op in p.tool_requirements.iter().cloned().chain(steps) {
            let names = out.entry(op).or_default();
            if !names.contains(&p.name) {
                names.push(p.name.clone());
            }
        }
    }
    out
}

/// What the runtime answers for `op` over `routes` of `env` (`EffectiveRoutes`): the stored row (own variant, else
/// `*`), else the R10 defaults (the built-ins, `case.create{handover|complaint}`), else not offered.
fn runtime_target(routes: &[OperationRoute], env: Env, op: &str) -> ResolvedTarget {
    let mine: Vec<OperationRoute> = routes
        .iter()
        .filter(|r| r.env == env.as_str())
        .cloned()
        .collect();
    if let Some(r) = route_for(&mine, op) {
        return target_of(&r.target, op);
    }
    DefaultRoutes::new().default_target(&RouteQuery {
        tenant: Uuid::nil(),
        job_id: Uuid::nil(),
        job_key: String::new(),
        env,
        op: op.to_owned(),
    })
}

fn names_text(names: &[String]) -> String {
    format!(
        "{} {}",
        if names.len() == 1 {
            "procedure"
        } else {
            "procedures"
        },
        names.join(", ")
    )
}

/// The procedure ops the runtime would NOT run in `env` over `routes` (not offered: no row and no default, or a
/// row that switches it off), each with its procedures. Built-ins and the default fallbacks are never listed.
pub fn unoffered_procedure_ops(
    routes: &[OperationRoute],
    env: Env,
    ops: &BTreeMap<String, Vec<String>>,
) -> BTreeMap<String, Vec<String>> {
    ops.iter()
        .filter(|(op, _)| matches!(runtime_target(routes, env, op), ResolvedTarget::NotOffered))
        .map(|(op, n)| (op.clone(), n.clone()))
        .collect()
}

/// Test env: one warning per procedure op the Test env does not offer (`<op>@test`; the test chat would hand the
/// case over `not_offered`, or the step would fail `no_route`). Never blocks publishing.
fn unrouted_test_procedure_ops(
    draft: &[OperationRoute],
    ops: &BTreeMap<String, Vec<String>>,
    href: &str,
) -> Vec<MissingItem> {
    unoffered_procedure_ops(draft, Env::Test, ops)
        .into_iter()
        .map(|(op, names)| MissingItem {
            dependency: "routing".to_string(),
            slot: format!("{op}@test"),
            detail: format!(
                "{PROCEDURE_DETAIL}{}: {op} has no route in the Test env, so a test chat that reaches {} hands the case over instead of running it. Route it for testing (does not block publishing)",
                names_text(&names),
                if names.len() == 1 { "it" } else { "them" },
            ),
            href: href.to_string(),
        })
        .collect()
}

/// Add to `expected` every operation the effective procedures of design `of` need (the same [`procedure_ops`] the
/// readiness reads) that the template's catalogue offers, as required operations: the admin can route them
/// (`set_route`) and the J6 matrix lists them. An operation the catalogue does not offer stays unexpected.
pub async fn add_procedure_ops(
    c: &mut PgConnection,
    reg: &Registry,
    compiler: &dyn TemplateCompiler,
    job: &crate::design::JobRef,
    of: &design::DesignOf,
    expected: &mut ExpectedCatalogue,
) -> ApiResult<()> {
    let procs = job_runtime::read(&mut *c, reg, compiler, job, of)
        .await?
        .procedures;
    for label in procedure_ops(&procs).keys() {
        let op = parse_op_key(label);
        if !op.object.is_empty() {
            expected.add_needed(&op);
        }
    }
    Ok(())
}

async fn assess(c: &mut PgConnection, cx: &ReadinessCx<'_>) -> ApiResult<Assessment> {
    let job = cx.job;
    let key = job.key.as_str();
    // the routes of the design checked (Production routes block apply) and of the draft (the Test env runs it)
    let checked = places::design_routes(&mut *c, cx.reg, job, &cx.design).await?;
    let draft = if cx.design == design::DesignOf::Draft {
        checked.clone()
    } else {
        places::design_routes(&mut *c, cx.reg, job, &design::DesignOf::Draft).await?
    };
    let row = routes_store::job_row(&mut *c, job.id).await?;
    let base = routes_store::view_of(&mut *c, row, ExpectedCatalogue::default(), Vec::new())
        .await?
        .data;
    let href = job_path(key, "/operations");
    let split = |r: &str| {
        let (op, env) = split_route(r);
        (op.to_string(), env.to_string())
    };
    // Fixture `missingFor`, hardened by R5: every required Production route that does not run in the design checked
    // blocks, including when the job has no route row at all (Production would refuse each call `no_route`). A job
    // that requires no route (uses no operation) has nothing here and publishes.
    // R5 also counts what the effective procedures of the design checked need (tool_requirements and step ops): the
    // orchestrator hands a case over before its first step when one is not offered. Ops the runtime runs by default
    // (built-ins, case.create{handover|complaint}) are not required to have a row.
    let checked_procs = job_runtime::read(&mut *c, cx.reg, cx.compiler, cx.job, &cx.design)
        .await?
        .procedures;
    let mut required = cx.requires.routes.clone();
    let prod_ops = unoffered_procedure_ops(&checked, Env::Prod, &procedure_ops(&checked_procs));
    let mut by_slot: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (op, names) in prod_ops {
        let slot = format!("{op}@prod");
        if !required.contains(&slot) {
            required.push(slot.clone());
        }
        by_slot.insert(slot, names);
    }
    let mut missing = unrouted_prod(&base, &checked, &required, &href);
    for m in &mut missing {
        if let Some(names) = by_slot.get(&m.slot) {
            m.detail = format!(
                "{} ({PROCEDURE_DETAIL}{})",
                m.detail.trim_end_matches('.'),
                names_text(names)
            );
        }
    }
    // Fixture `testEnvFor`: required Test-env routes that do not run in the draft (never blocking).
    let mut testing: Vec<MissingItem> = if draft.is_empty() {
        Vec::new()
    } else {
        cx.requires
            .test_routes
            .iter()
            .filter(|r| !route_runs(&base, &draft, r))
            .map(|r| {
                let (op, _) = split(r);
                MissingItem {
                    dependency: "routing".to_string(),
                    slot: r.clone(),
                    detail: format!(
                        "Operation {op} has no tested route on test: the test chat and regression runs cannot use it (does not block publishing)"
                    ),
                    href: href.clone(),
                }
            })
            .collect()
    };
    // What the test chat runs is the draft design: the procedures it would run that the Test env does not offer.
    let draft_procs = if cx.design == design::DesignOf::Draft {
        checked_procs
    } else {
        job_runtime::read(
            &mut *c,
            cx.reg,
            cx.compiler,
            cx.job,
            &design::DesignOf::Draft,
        )
        .await?
        .procedures
    };
    for item in unrouted_test_procedure_ops(&draft, &procedure_ops(&draft_procs), &href) {
        if !testing.iter().any(|t| t.slot == item.slot) {
            testing.push(item);
        }
    }
    if missing.is_empty() && testing.is_empty() && checked.is_empty() && draft.is_empty() {
        // nothing required and nothing routed anywhere yet: the area and its steps stay "Not checked yet"
        return Ok(Assessment {
            area: area_shell(AreaKey::Routes, key),
            blocking: missing,
            testing,
            go_live: Vec::new(),
            steps: Vec::new(),
        });
    }
    let slots = |xs: &[MissingItem]| {
        xs.iter()
            .map(|m| m.slot.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let n = required.len();
    let mut area = area_shell(AreaKey::Routes, key);
    area.template_part = format!(
        "{n} required operations in Production{}",
        if testing.is_empty() {
            ""
        } else {
            " (+ Test env for testing)"
        }
    );
    area.ready = u32::try_from(n.saturating_sub(missing.len())).unwrap_or(u32::MAX);
    area.total = u32::try_from(n).unwrap_or(u32::MAX);
    area.status = if !missing.is_empty() {
        AreaStatus::Gap
    } else if !testing.is_empty() {
        AreaStatus::Warn
    } else {
        AreaStatus::Ok
    };
    area.summary = if !missing.is_empty() {
        format!("Unrouted on prod: {}", slots(&missing))
    } else if !testing.is_empty() {
        format!(
            "Production routed \u{b7} Test env misses {} (blocks testing only)",
            slots(&testing)
        )
    } else {
        "Every required operation has a tested route".to_string()
    };
    let routes_step = if !missing.is_empty() {
        step(
            key,
            "routes",
            ReadinessStatus::Missing,
            format!("Unrouted on prod: {}", slots(&missing)),
        )
    } else if !testing.is_empty() {
        step(
            key,
            "routes",
            ReadinessStatus::Done,
            format!(
                "Production routed \u{b7} Test env: {} unrouted (blocks testing, not publishing)",
                slots(&testing)
            ),
        )
    } else {
        step(
            key,
            "routes",
            ReadinessStatus::Done,
            "Every required operation has a tested route or a fallback",
        )
    };
    let dor_step = if !missing.is_empty() {
        step(
            key,
            "dor_routes",
            ReadinessStatus::Missing,
            missing
                .iter()
                .map(|m| m.detail.as_str())
                .collect::<Vec<_>>()
                .join("; "),
        )
    } else if !testing.is_empty() {
        step(
            key,
            "dor_routes",
            ReadinessStatus::Done,
            format!(
                "All routed on prod; Test env still misses {} (testing only)",
                slots(&testing)
            ),
        )
    } else {
        step(key, "dor_routes", ReadinessStatus::Done, "All routed")
    };
    Ok(Assessment {
        area,
        blocking: missing,
        testing,
        go_live: Vec::new(),
        steps: vec![routes_step, dor_step],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn readiness_matches_the_ts_rules() {
        assert_eq!(op_readiness_of("green", false, false, false), "unrouted");
        assert_eq!(op_readiness_of("green", true, true, false), "stub");
        assert_eq!(op_readiness_of("red", true, false, true), "ready");
        assert_eq!(op_readiness_of("red", true, false, false), "blocked");
        assert_eq!(op_readiness_of("not_run", true, false, false), "untested");
    }

    fn builtin(object: &str, verb: &str) -> OperationRoute {
        OperationRoute {
            object: object.to_owned(),
            verb: verb.to_owned(),
            variant: "*".to_owned(),
            env: "prod".to_owned(),
            target: RouteTarget::Builtin,
            updated_by: "test".to_owned(),
            updated_at: String::new(),
        }
    }

    #[test]
    fn r5_unrouted_prod_op_blocks_even_without_route_rows() {
        let base = RouteData::default();
        let required = [
            "case.read@prod".to_owned(),
            "knowledge.search@prod".to_owned(),
        ];
        let missing = unrouted_prod(&base, &[], &required, "/admin/jobs/a/operations");
        let slots: Vec<&str> = missing.iter().map(|m| m.slot.as_str()).collect();
        assert_eq!(slots, ["case.read@prod", "knowledge.search@prod"]);
        assert!(missing.iter().all(|m| m.dependency == "routing"));
        assert!(
            missing[0].detail.contains("case.read") && missing[0].detail.contains("no_route"),
            "{}",
            missing[0].detail
        );

        // one routed to a target that runs: only the other blocks
        let routes = [builtin("knowledge", "search")];
        let missing = unrouted_prod(&base, &routes, &required, "/x");
        let slots: Vec<&str> = missing.iter().map(|m| m.slot.as_str()).collect();
        assert_eq!(slots, ["case.read@prod"]);
    }

    #[test]
    fn r5_tool_free_job_has_nothing_to_route() {
        assert!(unrouted_prod(&RouteData::default(), &[], &[], "/x").is_empty());
    }

    fn proc(name: &str, tools: &[&str], step_ops: &[(&str, Option<&str>)]) -> TemplateProcedure {
        let steps: Vec<Value> = step_ops
            .iter()
            .enumerate()
            .map(|(i, (op, variant))| {
                serde_json::json!({"step_number": i + 1, "phase": i, "name": format!("s{i}"),
                    "operation": op, "variant": variant, "modality": "observe", "side_effect": "R",
                    "identity_level": "2", "confirm_back": false, "gate": false, "args": {}})
            })
            .collect();
        serde_json::from_value(serde_json::json!({
            "name": name, "version": 1, "tool_requirements": tools, "step_defs": steps,
        }))
        .expect("a procedure")
    }

    fn route(object: &str, verb: &str, env: &str, target: RouteTarget) -> OperationRoute {
        OperationRoute {
            object: object.to_owned(),
            verb: verb.to_owned(),
            variant: "*".to_owned(),
            env: env.to_owned(),
            target,
            updated_by: "test".to_owned(),
            updated_at: String::new(),
        }
    }

    #[test]
    fn procedure_ops_collect_tool_requirements_and_step_ops_with_their_procedures() {
        let ps = [
            proc("refund", &["order.read"], &[("order.cancel", None)]),
            proc("track", &["order.read"], &[("case.create", Some("return"))]),
        ];
        let ops = procedure_ops(&ps);
        assert_eq!(ops["order.read"], ["refund", "track"]);
        assert_eq!(ops["order.cancel"], ["refund"]);
        assert_eq!(ops["case.create{return}"], ["track"]);
    }

    #[test]
    fn an_unrouted_procedure_op_is_a_test_warning_until_it_is_routed() {
        let ops = procedure_ops(&[proc("refund", &["order.read"], &[])]);
        let items = unrouted_test_procedure_ops(&[], &ops, "/admin/jobs/a/operations");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].slot, "order.read@test");
        assert_eq!(items[0].dependency, "routing");
        assert!(
            items[0].detail.starts_with(PROCEDURE_DETAIL),
            "{}",
            items[0].detail
        );
        assert!(
            items[0].detail.contains("procedure refund") && items[0].detail.contains("order.read"),
            "{}",
            items[0].detail
        );

        // a Prod route does not serve the Test env
        let prod = [route("order", "read", "prod", RouteTarget::AgentCases)];
        assert_eq!(unrouted_test_procedure_ops(&prod, &ops, "/x").len(), 1);
        // a row that switches it off is still not offered
        let off = [route("order", "read", "test", RouteTarget::NotOffered)];
        assert_eq!(unrouted_test_procedure_ops(&off, &ops, "/x").len(), 1);
        // routed in test: nothing to warn about
        let routed = [route("order", "read", "test", RouteTarget::AgentCases)];
        assert!(unrouted_test_procedure_ops(&routed, &ops, "/x").is_empty());
    }

    #[test]
    fn builtins_and_default_fallbacks_are_not_flagged() {
        let ops = procedure_ops(&[proc(
            "answer",
            &["knowledge.answer", "rule.evaluate{RET-01}"],
            &[
                ("knowledge.search", None),
                ("case.create", Some("handover")),
            ],
        )]);
        assert!(unrouted_test_procedure_ops(&[], &ops, "/x").is_empty());
        assert!(unoffered_procedure_ops(&[], Env::Prod, &ops).is_empty());
        // a variant is not a built-in
        let ops = procedure_ops(&[proc("p", &["knowledge.search{x}"], &[])]);
        assert_eq!(unoffered_procedure_ops(&[], Env::Test, &ops).len(), 1);
    }

    #[test]
    fn the_prod_view_of_a_procedure_op_follows_the_prod_rows() {
        let ops = procedure_ops(&[proc("refund", &[], &[("order.cancel", None)])]);
        assert_eq!(unoffered_procedure_ops(&[], Env::Prod, &ops).len(), 1);
        let test_only = [route("order", "cancel", "test", RouteTarget::AgentCases)];
        assert_eq!(
            unoffered_procedure_ops(&test_only, Env::Prod, &ops).len(),
            1
        );
        let prod = [route("order", "cancel", "prod", RouteTarget::AgentCases)];
        assert!(unoffered_procedure_ops(&prod, Env::Prod, &ops).is_empty());
    }

    #[test]
    fn a_procedure_op_the_catalogue_offers_becomes_a_required_expected_op() {
        let row = |key: &str, object: &str, verb: &str| -> agent_model::catalog::OperationRow {
            serde_json::from_value(serde_json::json!({
                "key": key, "object": object, "verb": verb, "variant": "*", "discriminator": null,
                "overridden": [], "inherited": [], "description": "", "retail": null, "stages": [],
                "error_states": [], "side_effect": "W"
            }))
            .expect("an operation row")
        };
        let mut e = ExpectedCatalogue {
            catalogue: vec![row("order.cancel", "order", "cancel")],
            ..ExpectedCatalogue::default()
        };
        assert!(e.add_needed(&parse_op_key("order.cancel")));
        assert!(
            e.find(&parse_op_key("order.cancel"))
                .is_some_and(|(o, _)| o.required)
        );
        assert_eq!(e.ops[0].side_effect, "W");
        assert!(e.add_needed(&parse_op_key("order.cancel")), "idempotent");
        assert_eq!(e.ops.len(), 1);
        // not offered by the catalogue: stays unexpected (operation_not_expected)
        assert!(!e.add_needed(&parse_op_key("order.teleport")));
        assert_eq!(e.ops.len(), 1);
    }

    #[test]
    fn job_keys_need_no_encoding() {
        assert_eq!(urlencoding_job("customer_care"), "customer_care");
        assert_eq!(urlencoding_job("a b"), "a%20b");
    }
}
