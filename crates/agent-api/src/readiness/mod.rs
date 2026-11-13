//! Release readiness (B2 T10, D67; spec B2 §2.6): a port of `web/src/lib/api/areas/readiness.ts` (`buildBoard`,
//! `releaseReadinessOf`, `blockingItem`) and of the fixture `readinessOf` / `boardOf` (`areas/release.ts`).
//!
//! Each area of the J3 board has at most one [`ReadinessContributor`] (its owning plan registers it in
//! [`registry()`]). The engine runs every contributor in [`AREA_ORDER`] over what is checked ([`Checked`]):
//!
//! - `blocking` items are the 409 `release_not_ready` list (`slot_not_deployed` / `route_not_ready`) and the board's
//!   `blocks: 'apply'` gaps;
//! - `testing` items are Test-env gaps (`blocks: 'testing'`, warning `test_route_not_ready`): never blocking;
//! - `go_live` gaps are warnings with the area's code (`rule_placeholder`, `store_unbound`, …): never blocking. D110
//!   (KH T8) makes required knowledge blocking: the knowledge contributor reports a required kind with nothing to
//!   resolve (or nothing in the primary language) as a blocking item (`dependency: knowledge`, code
//!   `knowledge_unbound`), per department (D115).
//!
//! An area with no contributor yet is `todo` / "Not checked yet" and never blocks; so is a checklist step no
//! contributor reports. B2 owns the steps `template`, `go_live` and `operate`, and the rules contributor
//! (crate::release::readiness_rules).
//!
//! Contributors read LOCAL STATE ONLY (no upstream HTTP: approve and apply make no network call) and read design only
//! through the read API (`crate::design::value` …) with [`ReadinessCx::design`].
pub mod board;
pub mod contract_tests;

use std::collections::BTreeMap;
use std::sync::Arc;

use agent_model::compile::Selection;
use chrono::{DateTime, SecondsFormat, Utc};
use futures_util::future::BoxFuture;
use serde::Serialize;
use serde_json::Value;
use sqlx::PgConnection;

use crate::design::hooks::is_template_refusal;
use crate::design::{DesignOf, JobRef, Registry};
use crate::error::{ApiError, ApiResult};
use crate::platform::templates::TemplatePin;
use crate::release::bundle::{Requires, RuleSlot};
use crate::release::compile::{CompileError, Compiled, TemplateCompiler};

// ---- the contract types (web/src/lib/api/types/release.ts) ---------------------------------------------------------

/// A board area (`ReadinessAreaKey`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AreaKey {
    Providers,
    Routes,
    Tests,
    Knowledge,
    Rules,
    Gates,
    Procedures,
    Toolsets,
}

/// The board's area order (fixture `AREA_ORDER`); contributors run in it.
pub const AREA_ORDER: [AreaKey; 8] = [
    AreaKey::Providers,
    AreaKey::Routes,
    AreaKey::Tests,
    AreaKey::Knowledge,
    AreaKey::Rules,
    AreaKey::Gates,
    AreaKey::Procedures,
    AreaKey::Toolsets,
];

impl AreaKey {
    pub fn key(self) -> &'static str {
        match self {
            AreaKey::Providers => "providers",
            AreaKey::Routes => "routes",
            AreaKey::Tests => "tests",
            AreaKey::Knowledge => "knowledge",
            AreaKey::Rules => "rules",
            AreaKey::Gates => "gates",
            AreaKey::Procedures => "procedures",
            AreaKey::Toolsets => "toolsets",
        }
    }

    fn rank(self) -> usize {
        AREA_ORDER
            .iter()
            .position(|k| *k == self)
            .unwrap_or(AREA_ORDER.len())
    }
}

/// One missing dependency of a release (`MissingItem`): a W5 slot (`dependency` = `strata_rag` …, `slot` = `embed`)
/// or a route (`dependency` = `routing`, `slot` = `case.read@prod`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MissingItem {
    pub dependency: String,
    pub slot: String,
    pub detail: String,
    pub href: String,
}

/// `ReadinessStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadinessStatus {
    Done,
    Current,
    Todo,
    Missing,
    Placeholder,
    Blocked,
}

/// One checklist step (`ReadinessStep`, 10 §5.2) or definition-of-ready line (`dor_*`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReadinessStep {
    pub key: String,
    pub label: String,
    pub status: ReadinessStatus,
    pub detail: String,
    pub href: String,
}

/// What a gap stops (`ReadinessBlocks`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Blocks {
    Apply,
    GoLive,
    Testing,
}

/// Who can fix a gap (`ReadinessFixer`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Fixer {
    TenantAdmin,
    SuperAdmin,
}

/// The build milestone from which apply enforces a check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Enforced {
    M1,
    M4,
}

/// `ReadinessGap`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReadinessGap {
    pub key: String,
    pub area: AreaKey,
    pub subject: String,
    pub title: String,
    pub why: String,
    pub cause: Option<String>,
    pub blocks: Blocks,
    pub fixer: Fixer,
    pub fix_label: String,
    pub href: String,
    pub enforced_from: Enforced,
}

/// `ReadinessArea['status']`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AreaStatus {
    Ok,
    Gap,
    Warn,
    Todo,
}

/// One board row (`ReadinessArea`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReadinessArea {
    pub key: AreaKey,
    pub label: String,
    pub question: String,
    pub template_part: String,
    pub tenant_part: String,
    pub ready: u32,
    pub total: u32,
    pub status: AreaStatus,
    pub summary: String,
    pub enforced_from: Enforced,
    pub page: String,
    pub href: String,
}

/// `ReadinessBoard['verdict']`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Blocked,
    GoLiveGaps,
    Ready,
    NoRelease,
}

/// `ReadinessBoard['release']`: the release checked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BoardRelease {
    pub id: String,
    pub hash: String,
    pub state: String,
    pub approved_by_name: Option<String>,
    pub approved_at: Option<String>,
}

/// `GET /api/jobs/{job}/readiness/board` (`ReadinessBoard`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReadinessBoard {
    pub job: String,
    pub release: Option<BoardRelease>,
    pub current_id: Option<String>,
    pub verdict: Verdict,
    pub gaps: Vec<ReadinessGap>,
    pub areas: Vec<ReadinessArea>,
    pub steps: Vec<ReadinessStep>,
}

/// `ReadinessCode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadinessCode {
    /// D106/D115 (KH T1): the selection checked has no industry, no department, a department of another industry
    /// or one twice, or there is no selection (`hierarchy_incomplete:selection`). Blocking; `subject` = `<code>:<path>`.
    HierarchyIncomplete,
    SlotNotDeployed,
    RouteNotReady,
    TestRouteNotReady,
    ContractTestsPending,
    /// D110/D115 (KH T8): blocking for a required kind of a department with no resolvable leaf, or nothing in the
    /// job's primary language (`subject` = `<department|job>:<stage>|<kind>[:<lang>]`); also the code of every
    /// knowledge-area warning (store, pack, other-language coverage).
    KnowledgeUnbound,
    RulePlaceholder,
    GatesUnreviewed,
    ProcedureUnpublished,
}

/// One readiness finding (`ReadinessItem`): the 409 `release_not_ready` `blocking` entries and the warnings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReadinessItem {
    pub code: ReadinessCode,
    pub subject: String,
    pub message: String,
    pub href: String,
}

/// `GET /api/jobs/{job}/release-readiness` (`ReleaseReadiness`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReleaseReadiness {
    pub job: String,
    pub release_id: Option<String>,
    pub routing_ready: bool,
    pub blocking: Vec<ReadinessItem>,
    pub warnings: Vec<ReadinessItem>,
}

// ---- contributors (§2.6) ------------------------------------------------------------------------------------------

/// `?of=`: the release checked (newest approved, else current) or the job's draft.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadOf {
    Release,
    Draft,
}

impl ReadOf {
    /// `release` (also when absent) or `draft`; anything else is 422 `invalid_of`.
    pub fn parse(of: Option<&str>) -> ApiResult<ReadOf> {
        match of {
            None | Some("release") => Ok(ReadOf::Release),
            Some("draft") => Ok(ReadOf::Draft),
            Some(_) => Err(ApiError::Unprocessable(
                "invalid_of",
                "of must be release or draft.".to_string(),
            )),
        }
    }
}

/// What a contributor checks. Read design ONLY through the read API with `design` (§2.4).
pub struct ReadinessCx<'a> {
    pub job: &'a JobRef,
    pub of: ReadOf,
    /// `Release(id)` for a cut or the current release, `Draft` for `of=draft` (and approve), `Live` when nothing was
    /// cut yet.
    pub design: DesignOf,
    pub selection: Option<&'a Selection>,
    /// `{slots, routes, test_routes}` of the compiled bundle checked (§5).
    pub requires: &'a Requires,
    /// Requirement (slot / route of `requires`) → the selection part that introduced it.
    pub causes: &'a BTreeMap<String, String>,
    /// The draft-compiled bundle's rule slots when this read compiled the draft ([`Checked::rules`]); `None` = read
    /// them from the release bundle (or compile the draft yourself).
    pub rules: Option<&'a [RuleSlot]>,
    /// The design registry (`AppState.design`), for the read API.
    pub reg: &'a Registry,
    /// The template compiler (`AppState.compiler`), for the draft-compiled bundle.
    pub compiler: &'a dyn TemplateCompiler,
}

/// One area's findings.
pub struct Assessment {
    /// The J3 board row (`key` is forced to the contributor's area).
    pub area: ReadinessArea,
    /// → 409 `release_not_ready` + gaps `blocks: 'apply'` (W5 slots, PRODUCTION routes).
    pub blocking: Vec<MissingItem>,
    /// Test-env gaps: warnings, `blocks: 'testing'`, never block a publish.
    pub testing: Vec<MissingItem>,
    /// `blocks: 'go_live'` gaps (`blocks` and `area` forced): apply passes, the agent runs with a hole.
    pub go_live: Vec<ReadinessGap>,
    /// Checklist steps and `dor_*` lines by key (see [`board::STEPS`] / [`board::DOR`]).
    pub steps: Vec<ReadinessStep>,
}

pub trait ReadinessContributor: Send + Sync + 'static {
    fn area(&self) -> AreaKey;
    /// LOCAL STATE ONLY: no upstream HTTP. Upstream facts are cached in the owner's tables and read here.
    fn assess<'a>(
        &'a self,
        c: &'a mut PgConnection,
        cx: &'a ReadinessCx<'a>,
    ) -> BoxFuture<'a, ApiResult<Assessment>>;
}

/// Every contributor's findings, merged in [`AREA_ORDER`].
#[derive(Debug, Clone, Default)]
pub struct Assessed {
    /// The blocking items, contributor order (providers before routes).
    pub missing: Vec<MissingItem>,
    pub test_env: Vec<MissingItem>,
    pub go_live: Vec<ReadinessGap>,
    /// One area per contributor that ran.
    pub areas: Vec<ReadinessArea>,
    pub steps: Vec<ReadinessStep>,
}

/// The board, plus the checklist pieces it is built from (fixture `ReadinessChecklist`).
#[derive(Debug, Clone)]
pub struct Evaluation {
    pub board: ReadinessBoard,
    pub missing: Vec<MissingItem>,
    pub test_env: Vec<MissingItem>,
    pub definition_of_ready: Vec<ReadinessStep>,
}

/// The registered contributors: at most one per area.
#[derive(Default)]
pub struct Readiness {
    contributors: Vec<Arc<dyn ReadinessContributor>>,
}

impl Readiness {
    /// Panics on a second contributor for one area (a programming error caught at boot).
    pub fn register(&mut self, c: Arc<dyn ReadinessContributor>) {
        let area = c.area();
        assert!(
            !self.contributors.iter().any(|x| x.area() == area),
            "a second readiness contributor for area `{}`",
            area.key()
        );
        self.contributors.push(c);
        self.contributors.sort_by_key(|x| x.area().rank());
    }

    /// The areas that have a contributor, in [`AREA_ORDER`].
    pub fn areas(&self) -> Vec<AreaKey> {
        self.contributors.iter().map(|x| x.area()).collect()
    }

    /// Run every contributor over `cx`.
    pub async fn assess(&self, c: &mut PgConnection, cx: &ReadinessCx<'_>) -> ApiResult<Assessed> {
        let mut out = Assessed::default();
        for k in &self.contributors {
            let area = k.area();
            let a = k.assess(&mut *c, cx).await?;
            out.missing.extend(a.blocking);
            out.test_env.extend(a.testing);
            out.go_live.extend(a.go_live.into_iter().map(|mut g| {
                g.area = area;
                g.blocks = Blocks::GoLive;
                g
            }));
            let mut row = a.area;
            row.key = area;
            out.areas.push(row);
            out.steps.extend(a.steps);
        }
        // D106/D115 (KH R2): a selection without the hierarchy blocks, it never reads as "requires nothing"
        let mut missing = match cx.selection {
            Some(sel) => hierarchy_missing(cx.job, sel),
            None => vec![no_selection(cx.job)],
        };
        missing.append(&mut out.missing);
        out.missing = missing;
        Ok(out)
    }

    /// What a publish of `cx` refuses on (blocking items only).
    pub async fn blocking(
        &self,
        c: &mut PgConnection,
        cx: &ReadinessCx<'_>,
    ) -> ApiResult<Vec<MissingItem>> {
        Ok(self.assess(c, cx).await?.missing)
    }

    /// The publish gate (D67): 409 `release_not_ready` with `blocking: ReadinessItem[]` when anything blocks.
    /// `release` names the release being applied (fixture apply: "`<id>` is not ready: …"); `None` is approve with
    /// `apply` (fixture: "Not approved: the release would not be ready (…).").
    pub async fn gate(
        &self,
        c: &mut PgConnection,
        cx: &ReadinessCx<'_>,
        release: Option<&str>,
    ) -> ApiResult<()> {
        let missing = self.blocking(c, cx).await?;
        if missing.is_empty() {
            return Ok(());
        }
        let names: Vec<String> = missing
            .iter()
            .map(|m| format!("{}.{}", m.dependency, m.slot))
            .collect();
        let message = match release {
            Some(id) => format!("{id} is not ready: {}.", names.join(", ")),
            None => format!(
                "Not approved: the release would not be ready ({}).",
                names.join(", ")
            ),
        };
        let items: Vec<Value> = missing
            .iter()
            .map(board::blocking_item)
            .map(|i| serde_json::to_value(i).map_err(|e| ApiError::Internal(e.into())))
            .collect::<ApiResult<_>>()?;
        Err(crate::release::release_not_ready(message, items))
    }

    /// The board, steps and definition of ready of `checked` (fixture `boardOf` + `readinessOf`).
    pub async fn evaluate(
        &self,
        c: &mut PgConnection,
        reg: &Registry,
        compiler: &dyn TemplateCompiler,
        checked: &Checked,
    ) -> ApiResult<Evaluation> {
        let cx = checked.cx(reg, compiler);
        let a = self.assess(c, &cx).await?;
        Ok(board::build(checked, a))
    }

    /// `readinessBoard`.
    pub async fn board(
        &self,
        c: &mut PgConnection,
        reg: &Registry,
        compiler: &dyn TemplateCompiler,
        checked: &Checked,
    ) -> ApiResult<ReadinessBoard> {
        Ok(self.evaluate(c, reg, compiler, checked).await?.board)
    }

    /// `releaseReadiness` (fixture `releaseReadinessOf`).
    pub async fn release_readiness(
        &self,
        c: &mut PgConnection,
        reg: &Registry,
        compiler: &dyn TemplateCompiler,
        checked: &Checked,
    ) -> ApiResult<ReleaseReadiness> {
        let ev = self.evaluate(c, reg, compiler, checked).await?;
        Ok(board::release_readiness(&ev.board, &ev.missing))
    }

    /// The blocking items of the job's DRAFT (its draft selection, `DesignOf::Draft`): what approve-with-apply
    /// refuses on (T11).
    pub async fn draft_blocking(
        &self,
        c: &mut PgConnection,
        reg: &Registry,
        compiler: &dyn TemplateCompiler,
        job: &JobRef,
    ) -> ApiResult<Vec<ReadinessItem>> {
        self.draft_blocking_with(c, reg, compiler, job, None).await
    }

    /// [`Self::draft_blocking`] over a draft compile the caller already made in this request (`Some(None)`: it
    /// compiled and the draft has no selection or the template refused it; `None`: compile here).
    pub async fn draft_blocking_with(
        &self,
        c: &mut PgConnection,
        reg: &Registry,
        compiler: &dyn TemplateCompiler,
        job: &JobRef,
        precompiled: Option<Option<Compiled>>,
    ) -> ApiResult<Vec<ReadinessItem>> {
        let checked = checked_with(&mut *c, compiler, job, ReadOf::Draft, precompiled).await?;
        let cx = checked.cx(reg, compiler);
        let missing = self.blocking(c, &cx).await?;
        Ok(missing.iter().map(board::blocking_item).collect())
    }
}

/// One blocking item per D106/D115 issue of `sel` (dependency `selection`, slot `<code>:<path>`, detail = the
/// issue's sentence): the publish gate refuses on them and the board lists them as apply gaps.
pub fn hierarchy_missing(job: &JobRef, sel: &Selection) -> Vec<MissingItem> {
    sel.hierarchy_issues()
        .into_iter()
        .map(|i| MissingItem {
            dependency: HIERARCHY_DEPENDENCY.to_owned(),
            slot: format!("{}:{}", i.code, i.path),
            detail: i.message,
            href: format!("/admin/jobs/{}/release", job.key),
        })
        .collect()
}

/// [`MissingItem::dependency`] of a [`hierarchy_missing`] item.
pub const HIERARCHY_DEPENDENCY: &str = "selection";

/// [`MissingItem::dependency`] of a required-knowledge item (D110/D115, KH T8, crate::knowledge::readiness): `slot` =
/// `<department|job>:<stage>|<kind>` (nothing resolves) or `…:<lang>` (the primary language is not covered); code
/// `knowledge_unbound`.
pub const KNOWLEDGE_DEPENDENCY: &str = "knowledge";

/// The blocking item of a job with no stored selection at all (KH T1 carried, D106/D115): it never reads as
/// "requires nothing".
pub fn no_selection(job: &JobRef) -> MissingItem {
    MissingItem {
        dependency: HIERARCHY_DEPENDENCY.to_owned(),
        slot: "hierarchy_incomplete:selection".to_owned(),
        detail:
            "The job has no selection: choose its industry and at least one of its departments."
                .to_owned(),
        href: format!("/admin/jobs/{}/release", job.key),
    }
}

/// The production contributors. Each plan adds one line here: `crate::<module>::readiness::register(&mut r);`.
/// Built once at boot into `AppState.readiness` (tests may replace that field with their own [`Readiness`]).
pub fn registry() -> Readiness {
    let mut r = Readiness::default();
    crate::release::readiness_rules::register(&mut r);
    crate::connectors::readiness::register(&mut r);
    contract_tests::register(&mut r);
    crate::knowledge::readiness::register(&mut r);
    crate::job_design::readiness::register(&mut r);
    r
}

// ---- what is checked ----------------------------------------------------------------------------------------------

/// What a readiness read checks (fixture `readinessOf` / `boardOf` inputs): the target release (newest approved,
/// else current), the design, and the requirements of the selection checked.
#[derive(Debug, Clone)]
pub struct Checked {
    pub job: JobRef,
    pub of: ReadOf,
    pub design: DesignOf,
    /// The release checked (newest approved, else current, or the one [`checked_release`] names); `None` = nothing
    /// cut yet.
    pub release: Option<BoardRelease>,
    pub current_id: Option<String>,
    pub selection: Option<Selection>,
    pub requires: Requires,
    pub causes: BTreeMap<String, String>,
    /// The rule slots (first of each id) of the draft-compiled bundle, when the draft selection was compiled for this
    /// read: contributors reuse it instead of compiling again. `None` for a release (its bundle holds them).
    pub rules: Option<Vec<RuleSlot>>,
    /// The job has template pins (checklist step `template`).
    pub pinned: bool,
}

impl Checked {
    pub fn cx<'a>(
        &'a self,
        reg: &'a Registry,
        compiler: &'a dyn TemplateCompiler,
    ) -> ReadinessCx<'a> {
        ReadinessCx {
            job: &self.job,
            of: self.of,
            design: self.design.clone(),
            selection: self.selection.as_ref(),
            requires: &self.requires,
            causes: &self.causes,
            rules: self.rules.as_deref(),
            reg,
            compiler,
        }
    }

    /// The release checked is an approved cut waiting to be applied.
    pub fn waiting(&self) -> bool {
        self.release.as_ref().is_some_and(|r| r.state == "approved")
    }
}

fn internal(what: String) -> ApiError {
    ApiError::Internal(anyhow::anyhow!(what))
}

#[derive(sqlx::FromRow)]
struct TargetRow {
    release_id: String,
    hash: String,
    state: String,
    approved_by: String,
    approved_at: DateTime<Utc>,
    selection: Value,
    requires: Value,
    causes: Value,
}

/// The job's current release id and template pins.
async fn job_row(c: &mut PgConnection, job: &JobRef) -> ApiResult<(Option<String>, Value)> {
    let row: (Option<String>, Value) =
        sqlx::query_as("SELECT current_release_id, pins FROM jobs WHERE id = $1")
            .bind(job.id)
            .fetch_one(&mut *c)
            .await?;
    Ok(row)
}

fn has_pins(pins: &Value) -> bool {
    pins.as_array().is_some_and(|a| !a.is_empty())
}

/// The board's view of release row `t` (approver's name: display name, else email, else their sub).
async fn board_release(c: &mut PgConnection, t: &TargetRow) -> ApiResult<BoardRelease> {
    let name = crate::members::names::one(&mut *c, &t.approved_by).await?;
    Ok(BoardRelease {
        id: t.release_id.clone(),
        hash: t.hash.clone(),
        state: t.state.clone(),
        approved_by_name: Some(name),
        approved_at: Some(t.approved_at.to_rfc3339_opts(SecondsFormat::AutoSi, true)),
    })
}

/// What checking release row `t` means: its stored selection / requires / causes, `DesignOf::Release(id)`.
async fn release_checked(
    c: &mut PgConnection,
    job: &JobRef,
    current_id: Option<String>,
    pinned: bool,
    t: TargetRow,
) -> ApiResult<Checked> {
    let release = board_release(&mut *c, &t).await?;
    // R15: a release cut before KH keeps the legacy shape; upgraded in memory
    let selection: Selection = crate::design::read::selection_of_value(&t.selection)
        .map_err(|e| internal(format!("job_release.selection is unreadable: {e}")))?;
    let requires: Requires = serde_json::from_value(t.requires)
        .map_err(|e| internal(format!("job_release.requires is unreadable: {e}")))?;
    let causes: BTreeMap<String, String> = serde_json::from_value(t.causes)
        .map_err(|e| internal(format!("job_release.causes is unreadable: {e}")))?;
    Ok(Checked {
        job: job.clone(),
        of: ReadOf::Release,
        design: DesignOf::Release(t.release_id),
        release: Some(release),
        current_id,
        selection: Some(selection),
        requires,
        causes,
        rules: None,
        pinned,
    })
}

/// Resolve what `of` checks for `job`, in the caller's job-scoped transaction:
/// - the target release = the newest approved release, else the current one;
/// - `of=release` with a target: that release (as [`checked_release`]);
/// - `of=release` without one: the draft selection's requirements, `DesignOf::Live` (the baseline);
/// - `of=draft`: the draft selection's requirements, `DesignOf::Draft`.
///
/// The draft selection compiles through `compiler` (once: its rule slots ride on [`Checked::rules`]); no selection,
/// or one the template refuses, requires nothing.
pub async fn checked(
    c: &mut PgConnection,
    compiler: &dyn TemplateCompiler,
    job: &JobRef,
    of: ReadOf,
) -> ApiResult<Checked> {
    checked_with(c, compiler, job, of, None).await
}

/// [`checked`] over a draft compile the caller already made (`Some(compiled)`, where `None` inside means no
/// selection or a template refusal); `None` compiles here.
pub async fn checked_with(
    c: &mut PgConnection,
    compiler: &dyn TemplateCompiler,
    job: &JobRef,
    of: ReadOf,
    precompiled: Option<Option<Compiled>>,
) -> ApiResult<Checked> {
    let (current_id, pins) = job_row(&mut *c, job).await?;
    let pinned = has_pins(&pins);
    let target = sqlx::query_as::<_, TargetRow>(
        "SELECT release_id, hash, state, approved_by, approved_at, selection, requires, causes \
           FROM job_release WHERE job_id = $1 AND state IN ('approved', 'current') \
          ORDER BY (state = 'approved') DESC, approved_at DESC, release_id DESC LIMIT 1",
    )
    .bind(job.id)
    .fetch_optional(&mut *c)
    .await?;
    let target = match target {
        Some(t) if of == ReadOf::Release => {
            return release_checked(&mut *c, job, current_id, pinned, t).await;
        }
        other => other,
    };
    let release = match &target {
        Some(t) => Some(board_release(&mut *c, t).await?),
        None => None,
    };
    let stored: Option<Option<Value>> =
        sqlx::query_scalar("SELECT selection FROM job_target WHERE job_id = $1")
            .bind(job.id)
            .fetch_optional(&mut *c)
            .await?;
    let selection: Option<Selection> = match stored.flatten() {
        Some(v) => Some(
            serde_json::from_value(v)
                .map_err(|e| internal(format!("job_target.selection is unreadable: {e}")))?,
        ),
        None => None,
    };
    let pins: Vec<TemplatePin> = serde_json::from_value(pins)
        .map_err(|e| internal(format!("jobs.pins is unreadable: {e}")))?;
    let draft = match precompiled {
        Some(done) => done.map(draft_of).unwrap_or_default(),
        None => draft_compile(&mut *c, compiler, selection.as_ref(), &pins).await?,
    };
    let design = match of {
        ReadOf::Draft => DesignOf::Draft,
        ReadOf::Release => DesignOf::Live,
    };
    Ok(Checked {
        job: job.clone(),
        of,
        design,
        release,
        current_id,
        selection,
        requires: draft.requires,
        causes: draft.causes,
        rules: Some(draft.rules),
        pinned,
    })
}

/// Check release `release_id` of `job` (any state: approved, current or applied earlier), e.g. for apply,
/// rollback and the rollback preview (T12): its stored selection / requires / causes, `DesignOf::Release(id)`,
/// `of = Release`. 404 `release_not_found` when the job has no such release.
pub async fn checked_release(
    c: &mut PgConnection,
    job: &JobRef,
    release_id: &str,
) -> ApiResult<Checked> {
    let (current_id, pins) = job_row(&mut *c, job).await?;
    let t = sqlx::query_as::<_, TargetRow>(
        "SELECT release_id, hash, state, approved_by, approved_at, selection, requires, causes \
           FROM job_release WHERE job_id = $1 AND release_id = $2",
    )
    .bind(job.id)
    .bind(release_id)
    .fetch_optional(&mut *c)
    .await?
    .ok_or_else(|| {
        ApiError::Missing(
            "release_not_found",
            format!("The job has no release {release_id}."),
        )
    })?;
    release_checked(&mut *c, job, current_id, has_pins(&pins), t).await
}

/// The draft-compiled bundle's requirements and rule slots (first of each id).
#[derive(Default)]
struct DraftCompiled {
    requires: Requires,
    causes: BTreeMap<String, String>,
    rules: Vec<RuleSlot>,
}

/// The requirements and first-of-each-id rule slots of a compiled draft bundle.
fn draft_of(compiled: Compiled) -> DraftCompiled {
    let b = compiled.bundle;
    let mut rules: Vec<RuleSlot> = Vec::new();
    for r in b.rules {
        if !rules.iter().any(|s| s.id == r.id) {
            rules.push(r);
        }
    }
    DraftCompiled {
        requires: b.requires,
        causes: b.causes,
        rules,
    }
}

/// Compile `sel` over `pins` (fixture `requiresFor` / `requirementCauses`); nothing selected, or a template refusal,
/// requires nothing. A selection without the D106/D115 hierarchy is not silent: [`Readiness::assess`] adds a
/// blocking [`hierarchy_missing`] item for each issue.
async fn draft_compile(
    c: &mut PgConnection,
    compiler: &dyn TemplateCompiler,
    sel: Option<&Selection>,
    pins: &[TemplatePin],
) -> ApiResult<DraftCompiled> {
    let Some(sel) = sel else {
        return Ok(DraftCompiled::default());
    };
    match compiler.compile(&mut *c, sel, pins).await {
        Ok(compiled) => Ok(draft_of(compiled)),
        Err(CompileError::Api(e)) if !is_template_refusal(&e) => Err(*e),
        Err(_) => Ok(DraftCompiled::default()),
    }
}
