//! B8 phase 2 T5 + T6: the job-scoped journey READS — `jobJourney` ([`job_journey`]) and `jobActivity`
//! ([`job_activity`]).
//!
//! **The journey** is the pinned template journey with the tenant's refinements over it.
//! - Template: [`grid::load_at`] per department of the job's selection (D115: `selection.departments`, the first is
//!   the primary), at the design's pins (`base::at`: the draft's, the live release's, or a release's), composed by
//!   [`merge`]: the primary's items win, a later department adds the stages, themes, activities and knowledge kinds
//!   the earlier ones do not have.
//! - Refinements (Q-R3 (a)): B2's `job_overlay_row` object `activity`, read through the design (`overlay` places of
//!   the draft, or of the live snapshot). `tombstone` hides a template activity, `override` relabels it (`label`;
//!   its `card` is the use case's), `extension` is the tenant's own activity (the whole activity, + `card`). A row
//!   whose base activity is gone (`base_removed`) shows nowhere; B2's conflict is how the tenant learns of it.
//! - `pending` per activity and the list: the draft ledger rows of kind `activity` (J4's) — and `overlay` rows of the
//!   object `activity` (J13's, until J13 hands the object over) — newest first. A pinned or live design has none.
//! - Findings are the JOB's: red per in-scope stage without an enabled activity (a hidden activity does not count),
//!   red per required knowledge kind with nothing to resolve (D110), amber per required operation without a route.
//!   The template's own findings are the designer's, not the tenant's.
//! - `outcomes` (B4 signals, 30 days, Production) only once the job has a current release.
//!
//! **The activity** ([`job_activity`]) is the use case of one activity of that journey (T6 table): its operations
//! with the catalogue attributes and the routes per env, the context space (B5's, else D61's), the gates and the
//! procedure of the space (R9, [`deps`]), the state changes its write operations cause (the compile's object
//! transitions), the live signals (B4), the lineage and the card (the pinned stack + the tenant's `card`).
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use agent_model::compile::{CatalogueItem, parse_department};
use agent_model::kinds::catalog::ObjectFields;
use chrono::{SecondsFormat, Utc};
use serde::Serialize;
use serde_json::{Map, Value, json};
use sqlx::PgConnection;

use super::deps::{
    self, Card, DialogueTurn, GateSubject, OperationRouteInfo, UseCaseGate, UseCaseProcedure,
};
use super::grid;
use super::model::{
    ANY_STAGE, Actor, ChannelKey, IndustryJourney, JourneyActivity, JourneyChannel, JourneyStage,
    JourneyTheme, ObjectRole, OperationExpectation, Severity, Slug, StageKnowledgeKind,
    StageMessage, ThemeStagePrior, ValidationFinding, object_key,
};
use crate::cases::signals::{self, ActivityOutcome, LiveCounts, OutcomeCount, RecentCase};
use crate::connectors::resolve::RouteData;
use crate::design::{self, DesignOf, JobRef, ledger};
use crate::error::{ApiError, ApiResult};
use crate::job_design::base::{self, Bases};
use crate::job_design::gates::stats;
use crate::job_design::op_label;
use crate::knowledge::kinds as knowledge_kinds_db;
use crate::platform::templates;
use crate::release::compile::job_key;
use crate::release::summary::{self, ReleaseRef};
use crate::state::AppState;

const ACTIVITY: &str = "activity";
const TOMBSTONE: &str = "tombstone";
const OVERRIDE: &str = "override";
const EXTENSION: &str = "extension";
/// The window of `outcomes` and of a use case's live numbers.
const OUTCOME_DAYS: i32 = 30;
const OUTCOME_WINDOW: &str = "last 30 days \u{b7} prod";
/// Recent cases a use case lists.
const RECENT_CASES: i64 = 5;

// ---- wire shapes (web `types/journey.ts`, 1:1) -------------------------------------------------------------------------

/// `RefinementState`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RefinementState {
    Base,
    Hidden,
    Extension,
    Relabelled,
}

/// `JobJourneyActivity.routing`: the required operations and those without a route (`case.read \u{b7} test`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Routing {
    pub required: usize,
    pub unrouted: Vec<String>,
}

/// `JobJourneyActivity`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct JobJourneyActivity {
    #[serde(flatten)]
    pub activity: JourneyActivity,
    pub refinement: RefinementState,
    /// The template's label when relabelled.
    pub base_label: Option<String>,
    /// A refinement of it is in the draft ledger, not yet in a release.
    pub pending: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub routing: Option<Routing>,
}

/// `JobKnowledgeKind`: a stage's knowledge kind joined with the job's binding.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct JobKnowledgeKind {
    #[serde(flatten)]
    pub base: StageKnowledgeKind,
    /// The store path the kind is bound to; null when unbound.
    pub store: Option<String>,
    /// Something resolves it: the bound tenant store, or (unless it is the tenant's own data) a live pack.
    pub present: bool,
}

/// `PendingRefinement`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PendingRefinement {
    pub at: String,
    pub by: String,
    pub what: String,
    pub activity: Option<String>,
}

/// `JobJourney.outcomes`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Outcomes {
    pub window: String,
    pub rows: Vec<ActivityOutcome>,
}

/// `GET /api/jobs/{job}/journey` (web `JobJourney`).
#[derive(Clone, Debug, Serialize)]
pub struct JobJourney {
    pub job: String,
    pub industry: String,
    pub industry_label: String,
    /// `layer@vN` per pin.
    pub pins: Vec<String>,
    pub release: Option<ReleaseRef>,
    pub stages: Vec<JourneyStage>,
    pub themes: Vec<JourneyTheme>,
    pub channels: Vec<JourneyChannel>,
    pub activities: Vec<JobJourneyActivity>,
    pub messages: Vec<StageMessage>,
    pub knowledge_kinds: Vec<JobKnowledgeKind>,
    pub pending: Vec<PendingRefinement>,
    pub findings: Vec<ValidationFinding>,
    pub priors: Vec<ThemeStagePrior>,
    pub outcomes: Option<Outcomes>,
}

/// `ActivityOperation`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ActivityOperation {
    pub op: String,
    pub variant: Option<String>,
    pub required: bool,
    pub modality: &'static str,
    pub side_effect: &'static str,
    pub identity_level: String,
    pub routes: Vec<OperationRouteInfo>,
}

/// `ActivityDetail.unrouted_required[]`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct UnroutedOp {
    pub op: String,
    pub env: String,
}

/// `ActivityUseCase.identity`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Identity {
    pub level: u8,
    pub label: &'static str,
    pub how: &'static str,
}

/// `UseCaseStateChange`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct UseCaseStateChange {
    pub object: String,
    pub verb: String,
    pub from: Vec<String>,
    pub to: Option<String>,
    pub guard: Option<String>,
}

/// `ActivityUseCase.checks[]`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct UseCaseCheck {
    pub key: &'static str,
    pub text: String,
}

/// `UseCaseLayer`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct UseCaseLayer {
    /// `base | industry | department | job | tenant`.
    pub kind: &'static str,
    pub tag: Option<String>,
    pub contributes: String,
    /// F-44, on the `tenant` layer only: whether the use case card is the template's or the tenant's own
    /// refinement of it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub card: Option<&'static str>,
}

/// `ActivityUseCase.live_vs_draft`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct LiveVsDraft {
    pub live: String,
    pub draft: String,
}

/// `ActivityUseCase` (J5).
#[derive(Clone, Debug, Serialize)]
pub struct ActivityUseCase {
    pub summary: String,
    pub identity: Identity,
    pub examples: Vec<String>,
    pub gates: Vec<UseCaseGate>,
    pub procedure: Option<UseCaseProcedure>,
    pub state_changes: Vec<UseCaseStateChange>,
    pub checks: Vec<UseCaseCheck>,
    pub live: Option<LiveCounts>,
    pub outcomes: Vec<OutcomeCount>,
    pub recent_cases: Vec<RecentCase>,
    pub lineage: Vec<UseCaseLayer>,
    pub pending: Vec<PendingRefinement>,
    pub live_vs_draft: Option<LiveVsDraft>,
    pub draft_rev: i64,
}

/// `GET /api/jobs/{job}/journey/activities/{key}` (web `ActivityDetail`).
#[derive(Clone, Debug, Serialize)]
pub struct ActivityDetail {
    pub activity: JobJourneyActivity,
    pub stage: JourneyStage,
    pub operations: Vec<ActivityOperation>,
    /// D61 `<industry>:<area>:<slug>`; null when the activity belongs to no area yet.
    pub context_space: Option<String>,
    pub procedure: String,
    pub sample_dialogue: Vec<DialogueTurn>,
    pub success_example: Value,
    pub error_example: Value,
    pub metric: Value,
    pub knowledge_kinds: Vec<JobKnowledgeKind>,
    pub unrouted_required: Vec<UnroutedOp>,
    pub use_case: ActivityUseCase,
}

// ---- the tenant's refinements -------------------------------------------------------------------------------------------

/// One `job_overlay_row` of the object `activity`.
#[derive(Clone, Debug, PartialEq)]
pub struct OverlayRow {
    /// `tombstone | override | extension`.
    pub kind: String,
    pub item: Option<Map<String, Value>>,
}

/// The tenant's `activity` overlay rows of design `of`, by activity key.
pub async fn overlay_rows(
    c: &mut PgConnection,
    st: &AppState,
    job: &JobRef,
    of: &DesignOf,
) -> ApiResult<BTreeMap<String, OverlayRow>> {
    let rows = design::values_of_kind(&mut *c, &st.design, job, of, "activity").await?;
    let mut out = BTreeMap::new();
    for r in rows {
        let Some(key) = r.r#ref.get("key").and_then(Value::as_str) else {
            continue;
        };
        let Some(v) = r.snap.as_ref().and_then(Value::as_object) else {
            continue;
        };
        out.insert(
            key.to_owned(),
            OverlayRow {
                kind: v
                    .get("kind")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                item: v.get("item").and_then(Value::as_object).cloned(),
            },
        );
    }
    Ok(out)
}

fn iso(t: chrono::DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::AutoSi, true)
}

/// The job's unpublished activity changes, newest first (module docs).
async fn pending_of(c: &mut PgConnection, job: &JobRef) -> ApiResult<Vec<PendingRefinement>> {
    let mut rows: Vec<_> = ledger::of_job(&mut *c, job)
        .await?
        .into_iter()
        .filter_map(|r| {
            let key = match r.kind.as_str() {
                ACTIVITY => r.r#ref.get("key"),
                "overlay" if r.r#ref.get("object").and_then(Value::as_str) == Some(ACTIVITY) => {
                    r.r#ref.get("key")
                }
                _ => None,
            }
            .and_then(Value::as_str)?
            .to_owned();
            Some((
                r.at,
                PendingRefinement {
                    at: iso(r.at),
                    by: r.by_sub.clone(),
                    what: r.label.clone(),
                    activity: Some(key),
                },
            ))
        })
        .collect();
    rows.sort_by_key(|r| std::cmp::Reverse(r.0));
    Ok(rows.into_iter().map(|(_, p)| p).collect())
}

// ---- composing the template journeys --------------------------------------------------------------------------------------

/// The journeys of the job's departments composed (D115).
struct Merged {
    industry: String,
    label: String,
    stages: Vec<JourneyStage>,
    themes: Vec<JourneyTheme>,
    channels: Vec<JourneyChannel>,
    priors: Vec<ThemeStagePrior>,
    messages: Vec<StageMessage>,
    activities: Vec<JourneyActivity>,
    kinds: Vec<StageKnowledgeKind>,
}

/// Append the items of `more` whose key `key` has not been seen.
fn union<T, K: Ord>(into: &mut Vec<T>, more: Vec<T>, key: impl Fn(&T) -> K) {
    let mut seen: BTreeSet<K> = into.iter().map(&key).collect();
    for x in more {
        if seen.insert(key(&x)) {
            into.push(x);
        }
    }
}

/// The primary department's journey, then each later one's additions. A stage is out of scope only when no
/// department that has it keeps it in scope. `None` for no journey at all.
fn merge(grids: Vec<IndustryJourney>) -> Option<Merged> {
    let mut it = grids.into_iter();
    let first = it.next()?;
    let mut m = Merged {
        industry: first.industry,
        label: first.label,
        stages: first.stages,
        themes: first.themes,
        channels: first.channels,
        priors: first.priors,
        messages: first.messages,
        activities: first.activities,
        kinds: first.knowledge_kinds,
    };
    for g in it {
        for s in &g.stages {
            if let Some(mine) = m.stages.iter_mut().find(|x| x.key == s.key) {
                mine.out_of_scope &= s.out_of_scope;
            }
        }
        union(&mut m.stages, g.stages, |s| s.key.as_str().to_owned());
        union(&mut m.themes, g.themes, |t| t.key.as_str().to_owned());
        union(&mut m.channels, g.channels, |c| c.key as u8);
        union(&mut m.priors, g.priors, |p| {
            (p.theme.as_str().to_owned(), p.stage.as_str().to_owned())
        });
        union(&mut m.messages, g.messages, |x| {
            (x.stage.as_str().to_owned(), x.channel as u8)
        });
        union(&mut m.activities, g.activities, |a| a.key.clone());
        union(&mut m.kinds, g.knowledge_kinds, |k| {
            (k.stage.clone(), k.kind.clone())
        });
    }
    m.stages
        .sort_by(|a, b| (a.order, a.key.as_str()).cmp(&(b.order, b.key.as_str())));
    m.themes
        .sort_by(|a, b| (a.order, a.key.as_str()).cmp(&(b.order, b.key.as_str())));
    let pos: BTreeMap<String, usize> = m
        .stages
        .iter()
        .enumerate()
        .map(|(i, s)| (s.key.as_str().to_owned(), i))
        .collect();
    let at = |a: &JourneyActivity| pos.get(a.stage.as_str()).copied().unwrap_or(usize::MAX);
    m.activities
        .sort_by(|a, b| (at(a), a.key.as_str()).cmp(&(at(b), b.key.as_str())));
    Some(m)
}

fn missing_journey(job: &JobRef) -> ApiError {
    ApiError::Missing(
        "journey_not_found",
        format!("Job “{}” has no journey template pinned.", job.key),
    )
}

/// One grid per department of the selection, at the design's pins.
async fn grids(
    c: &mut PgConnection,
    bases: &Bases,
    job: &JobRef,
) -> ApiResult<Vec<IndustryJourney>> {
    let sel = &bases.selection;
    if sel.industry.is_empty() {
        return Err(missing_journey(job));
    }
    let mut out = Vec::new();
    for key in &sel.departments {
        let Some((industry, department)) = parse_department(key) else {
            continue;
        };
        if industry != sel.industry {
            continue;
        }
        out.push(grid::load_at(&mut *c, &bases.pins, sel, &department).await?);
    }
    Ok(out)
}

// ---- overlaying the tenant's rows --------------------------------------------------------------------------------------

/// An extension row's activity: the whole activity as the tenant wrote it, else (an item that does not read as one)
/// the key, its label and the stage of the key.
fn extension_activity(key: &str, item: &Map<String, Value>) -> Option<JourneyActivity> {
    let mut whole = item.clone();
    whole.remove("card");
    whole
        .entry("key".to_owned())
        .or_insert_with(|| Value::String(key.to_owned()));
    if let Ok(a) = serde_json::from_value::<JourneyActivity>(Value::Object(whole)) {
        return Some(a);
    }
    let stage = Slug::new(key.split_once(':')?.0)?;
    tracing::warn!(
        activity = key,
        "an extension activity row is not a whole activity; shown with its label only"
    );
    Some(JourneyActivity {
        key: key.to_owned(),
        stage,
        actor: Actor::Customer,
        label: item
            .get("label")
            .and_then(Value::as_str)
            .unwrap_or(key)
            .to_owned(),
        trigger: String::new(),
        also_in_stages: Vec::new(),
        objects: Vec::new(),
        operations: Vec::new(),
        summary: item
            .get("summary")
            .and_then(Value::as_str)
            .map(str::to_owned),
        description: item
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_owned),
        area: item.get("area").and_then(Value::as_str).map(str::to_owned),
        layer: None,
        required: false,
    })
}

/// The template's activities with the tenant's rows over them, then the tenant's extensions. A template activity
/// with no operations of its own takes the base bundle's (`Bases::activities`, the seed's).
fn overlay_activities(
    base: Vec<JourneyActivity>,
    rows: &BTreeMap<String, OverlayRow>,
    pending: &BTreeSet<String>,
    bases: &Bases,
) -> Vec<JobJourneyActivity> {
    let known: BTreeSet<String> = base.iter().map(|a| a.key.clone()).collect();
    let mut out = Vec::new();
    for mut a in base {
        if a.operations.is_empty()
            && let Some(b) = bases.activity(&a.key)
        {
            a.operations = b
                .ops
                .iter()
                .map(|o| OperationExpectation {
                    op: o.op.clone(),
                    variant: o.variant.clone(),
                    required: o.required,
                })
                .collect();
        }
        let is_pending = pending.contains(&a.key);
        let (refinement, base_label) = match rows.get(&a.key) {
            Some(r) if r.kind == TOMBSTONE => (RefinementState::Hidden, None),
            Some(r) if r.kind == OVERRIDE => {
                let label = r
                    .item
                    .as_ref()
                    .and_then(|i| i.get("label"))
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .filter(|l| !l.is_empty() && *l != a.label);
                match label {
                    Some(l) => (
                        RefinementState::Relabelled,
                        Some(std::mem::replace(&mut a.label, l)),
                    ),
                    None => (RefinementState::Base, None),
                }
            }
            _ => (RefinementState::Base, None),
        };
        out.push(JobJourneyActivity {
            activity: a,
            refinement,
            base_label,
            pending: is_pending,
            routing: None,
        });
    }
    for (key, r) in rows {
        if r.kind != EXTENSION || known.contains(key) {
            continue;
        }
        let Some(act) = r.item.as_ref().and_then(|i| extension_activity(key, i)) else {
            continue;
        };
        out.push(JobJourneyActivity {
            activity: act,
            refinement: RefinementState::Extension,
            base_label: None,
            pending: pending.contains(key),
            routing: None,
        });
    }
    out
}

// ---- knowledge kinds + findings ------------------------------------------------------------------------------------------

/// The journey's kinds joined with the job's bindings: `store` is the bound path; `present` is whether anything
/// resolves it (D110, `knowledge::kinds::leaf_set` of the design). `required` is the template's or the job row's.
async fn job_kinds(
    c: &mut PgConnection,
    st: &AppState,
    job: &JobRef,
    of: &DesignOf,
    kinds: Vec<StageKnowledgeKind>,
) -> ApiResult<Vec<JobKnowledgeKind>> {
    let rows = knowledge_kinds_db::rows(&mut *c, job.id).await?;
    let mut out = Vec::with_capacity(kinds.len());
    for k in kinds {
        let row = rows.iter().find(|r| r.stage == k.stage && r.kind == k.kind);
        let binding = design::value(
            &mut *c,
            &st.design,
            job,
            of,
            "knowledge",
            &knowledge_kinds_db::kind_ref(&k.stage, &k.kind),
        )
        .await?;
        let store = binding
            .as_ref()
            .map(knowledge_kinds_db::value_parts)
            .transpose()?
            .and_then(|(store, _)| store);
        let mut present = false;
        if let Some(r) = row {
            present =
                match knowledge_kinds_db::leaf_set(&mut *c, &st.design, job, &k.stage, &k.kind, of)
                    .await
                {
                    Ok(ls) => {
                        ls.tenant.is_some() || (!r.tenant_data && ls.packs.iter().any(|p| p.live()))
                    }
                    Err(ApiError::Missing("kind_not_found", _)) => false,
                    Err(e) => return Err(e),
                };
        }
        out.push(JobKnowledgeKind {
            store,
            present,
            base: StageKnowledgeKind {
                required: k.required || row.is_some_and(|r| r.required),
                ..k
            },
        });
    }
    Ok(out)
}

fn finding(
    rule: u8,
    severity: Severity,
    message: String,
    stage: Option<String>,
    activity: Option<String>,
) -> ValidationFinding {
    ValidationFinding {
        rule,
        severity,
        message,
        stage,
        activity,
    }
}

/// The job's findings (module docs).
fn findings_of(
    stages: &[JourneyStage],
    activities: &[JobJourneyActivity],
    kinds: &[JobKnowledgeKind],
) -> Vec<ValidationFinding> {
    let mut out = Vec::new();
    for s in stages.iter().filter(|s| !s.out_of_scope) {
        let enabled = activities
            .iter()
            .any(|a| a.activity.stage == s.key && a.refinement != RefinementState::Hidden);
        if !enabled {
            out.push(finding(
                4,
                Severity::Red,
                format!("stage {} has no enabled activity", s.label),
                Some(s.key.as_str().to_owned()),
                None,
            ));
        }
    }
    for k in kinds.iter().filter(|k| k.base.required && !k.present) {
        out.push(finding(
            0,
            Severity::Red,
            format!(
                "required knowledge kind {} ({}) has no knowledge: bind a store or pin a platform pack",
                k.base.kind, k.base.stage
            ),
            (k.base.stage != ANY_STAGE).then(|| k.base.stage.clone()),
            None,
        ));
    }
    for a in activities
        .iter()
        .filter(|a| a.refinement != RefinementState::Hidden)
    {
        for u in a.routing.iter().flat_map(|r| r.unrouted.iter()) {
            out.push(finding(
                0,
                Severity::Amber,
                format!(
                    "{}: required {} has no route",
                    a.activity.key,
                    u.replace(" \u{b7} ", " in ")
                ),
                Some(a.activity.stage.as_str().to_owned()),
                Some(a.activity.key.clone()),
            ));
        }
    }
    out
}

// ---- the journey ---------------------------------------------------------------------------------------------------------

/// Everything [`job_journey`] read, for [`job_activity`] to build on.
struct Built {
    journey: JobJourney,
    bases: Arc<Bases>,
    routes: RouteData,
    rows: BTreeMap<String, OverlayRow>,
    industry: String,
}

async fn build(
    c: &mut PgConnection,
    st: &AppState,
    job: &JobRef,
    of: &DesignOf,
) -> ApiResult<Built> {
    let bases = base::at(&mut *c, &st.design, st.compiler.as_ref(), job, of).await?;
    let m = merge(grids(&mut *c, &bases, job).await?).ok_or_else(|| missing_journey(job))?;
    let rows = overlay_rows(&mut *c, st, job, of).await?;
    let pending = if *of == DesignOf::Draft {
        pending_of(&mut *c, job).await?
    } else {
        Vec::new()
    };
    let pending_keys: BTreeSet<String> =
        pending.iter().filter_map(|p| p.activity.clone()).collect();
    let routes = deps::route_data(&mut *c, &st.design, job, of).await?;

    let mut activities = overlay_activities(m.activities, &rows, &pending_keys, &bases);
    for a in &mut activities {
        let ops = &a.activity.operations;
        a.routing = Some(Routing {
            required: ops.iter().filter(|o| o.required).count(),
            unrouted: deps::unrouted_required(&routes, ops)
                .into_iter()
                .map(|(op, env)| format!("{op} \u{b7} {env}"))
                .collect(),
        });
    }
    let kinds = job_kinds(&mut *c, st, job, of, m.kinds).await?;
    let findings = findings_of(&m.stages, &activities, &kinds);

    let release = summary::current_ref(&mut *c, job).await?;
    let outcomes = if release.is_some() {
        Some(Outcomes {
            window: OUTCOME_WINDOW.to_owned(),
            rows: signals::activity_outcomes(&mut *c, job, OUTCOME_DAYS).await?,
        })
    } else {
        None
    };
    let journey = JobJourney {
        job: job.key.clone(),
        industry: m.industry.clone(),
        industry_label: m.label,
        pins: bases
            .pins
            .iter()
            .map(|p| format!("{}@v{}", p.layer, p.version))
            .collect(),
        release,
        stages: m.stages,
        themes: m.themes,
        channels: m.channels,
        activities,
        messages: m.messages,
        knowledge_kinds: kinds,
        pending,
        findings,
        priors: m.priors,
        outcomes,
    };
    Ok(Built {
        journey,
        bases,
        routes,
        rows,
        industry: m.industry,
    })
}

/// The job's journey `of` the draft (the pages' view), the live release or a release: the pinned template journeys
/// of its departments (D115) with the tenant's activity refinements over them (module docs). 404
/// `journey_not_found` when the design selects no industry or department.
pub async fn job_journey(
    c: &mut PgConnection,
    st: &AppState,
    job: &JobRef,
    of: &DesignOf,
) -> ApiResult<JobJourney> {
    Ok(build(c, st, job, of).await?.journey)
}

/// The activities of the job's journey `of` the draft, the live release or a release that are on (not hidden), each
/// with its use-case card over the pinned stack (`None` = no layer gives one) — what the draft validation compiles
/// its scenarios from (B8-4). A job whose design selects no industry or department has none (no 404 here).
pub async fn enabled_cards(
    c: &mut PgConnection,
    st: &AppState,
    job: &JobRef,
    of: &DesignOf,
) -> ApiResult<Vec<(JobJourneyActivity, Option<Card>)>> {
    let b = match build(&mut *c, st, job, of).await {
        Ok(b) => b,
        Err(ApiError::Missing("journey_not_found", _)) => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut cards = deps::pinned_cards(&mut *c, &b.bases.pins, &b.bases.selection).await?;
    Ok(b.journey
        .activities
        .into_iter()
        .filter(|a| a.refinement != RefinementState::Hidden)
        .map(|a| {
            let card = cards.remove(&a.activity.key);
            (a, card)
        })
        .collect())
}

// ---- the activity use case (J5) ----------------------------------------------------------------------------------------------

const IDENTITY: [Identity; 4] = [
    Identity {
        level: 0,
        label: "Public",
        how: "Anyone may ask; nothing about the customer is needed.",
    },
    Identity {
        level: 1,
        label: "Holds an identifier",
        how: "The customer is recognised only by a channel contact on a signed-in conversation; the agent never asks for a reference, and no address or personal fields come back.",
    },
    Identity {
        level: 2,
        label: "Verified",
        how: "The customer is already verified (KYC) before the conversation starts; the agent loads their records itself and never asks for any identifier.",
    },
    Identity {
        level: 3,
        label: "Step-up",
        how: "A one-time code or MFA before the operation runs.",
    },
];

/// The highest digit of a display level (`2`, `1→2`; `S` and `—` read as 0).
fn level_of(s: &str) -> u8 {
    s.chars()
        .filter_map(|c| c.to_digit(10))
        .max()
        .map_or(0, |d| u8::try_from(d).unwrap_or(0))
}

fn verb_word(verb: &str) -> String {
    match verb {
        "read" => "reads",
        "list" => "looks up",
        "search" => "searches",
        "create" => "opens",
        "cancel" => "cancels",
        "modify" => "changes",
        "update" => "updates",
        "redirect" => "redirects",
        "report_problem" => "reports a problem on",
        "refund" => "refunds",
        "verify" => "verifies",
        "evaluate" => "evaluates",
        "send" => "sends",
        "comment" => "comments on",
        other => other,
    }
    .to_owned()
}

/// Plain language for what happens when a customer wants this (the fixture's `summaryOf`).
fn summary_of(a: &JourneyActivity, ops: &[ActivityOperation], writes: bool) -> String {
    if ops.is_empty() {
        let side = if a.actor == Actor::Company {
            "Company-side"
        } else {
            "Content"
        };
        return format!(
            "{side} activity: the agent answers from the knowledge store; no system is called."
        );
    }
    let phrase = |o: &ActivityOperation| {
        let (obj, verb) = o.op.split_once('.').unwrap_or((o.op.as_str(), ""));
        format!(
            "{} the {}",
            verb_word(verb),
            object_key(obj).replace('_', " ")
        )
    };
    let reads: Vec<String> = ops
        .iter()
        .filter(|o| o.side_effect == "R")
        .map(phrase)
        .collect();
    let changes: Vec<String> = ops
        .iter()
        .filter(|o| o.side_effect != "R")
        .map(phrase)
        .collect();
    let who = if a.actor != Actor::Company {
        let says = if a.trigger.is_empty() {
            format!("\u{201c}{}\u{201d}", a.label.to_lowercase())
        } else {
            a.trigger.clone()
        };
        let speaker = if a.actor == Actor::Employee {
            "An employee"
        } else {
            "A customer"
        };
        format!("{speaker} says {says}.")
    } else {
        format!("{}.", a.trigger)
    };
    let r = if reads.is_empty() {
        " The agent".to_owned()
    } else {
        format!(" The agent {}", reads.join(" and "))
    };
    let c = if changes.is_empty() {
        ". It changes nothing.".to_owned()
    } else {
        format!(
            "{} {} once the customer confirms.",
            if reads.is_empty() { "" } else { ", then" },
            changes.join(" and ")
        )
    };
    let text = format!(
        "{who}{r}{}",
        if writes {
            c.as_str()
        } else {
            ". It changes nothing."
        }
    );
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The status changes the write operations `writes` (`object.verb`) may cause: the compile's object transitions of
/// each verb, grouped by target status; a write with no transition leaves the status as it is.
fn state_changes(writes: &[String], objects: &[CatalogueItem]) -> Vec<UseCaseStateChange> {
    let mut out = Vec::new();
    for verb in writes {
        let object = verb.split('.').next().unwrap_or(verb);
        let transitions = objects
            .iter()
            .find(|o| o.key == object)
            .and_then(|o| {
                serde_json::from_value::<ObjectFields>(Value::Object(o.fields.clone())).ok()
            })
            .map(|f| f.transitions)
            .unwrap_or_default();
        let mut rows: Vec<UseCaseStateChange> = Vec::new();
        for t in transitions.iter().filter(|t| t.verb == *verb) {
            let to = Some(t.to_status.clone()).filter(|s| !s.is_empty());
            match rows.iter().position(|r| r.to == to) {
                Some(i) => {
                    if !rows[i].from.contains(&t.from_status) {
                        rows[i].from.push(t.from_status.clone());
                    }
                }
                None => rows.push(UseCaseStateChange {
                    object: object.to_owned(),
                    verb: verb.clone(),
                    from: vec![t.from_status.clone()],
                    to,
                    guard: t.guard_rule.clone(),
                }),
            }
        }
        if rows.is_empty() {
            rows.push(UseCaseStateChange {
                object: object.to_owned(),
                verb: verb.clone(),
                from: Vec::new(),
                to: None,
                guard: None,
            });
        }
        out.extend(rows);
    }
    out
}

/// How customers are offered the activity under `row`.
fn shown(row: Option<&OverlayRow>, own_label: &str) -> String {
    match row {
        Some(r) if r.kind == TOMBSTONE => "hidden".to_owned(),
        Some(r) => {
            let label = r
                .item
                .as_ref()
                .and_then(|i| i.get("label"))
                .and_then(Value::as_str)
                .filter(|l| !l.is_empty())
                .unwrap_or(own_label);
            format!("\u{201c}{label}\u{201d}")
        }
        None => format!("\u{201c}{own_label}\u{201d}"),
    }
}

/// D62: how customers are offered the activity in the live release vs in the draft; `None` when they agree.
fn live_vs_draft(
    live: Option<&OverlayRow>,
    draft: Option<&OverlayRow>,
    template_label: &str,
) -> Option<LiveVsDraft> {
    let is_ext = |r: Option<&OverlayRow>| r.is_some_and(|r| r.kind == EXTENSION);
    let out = LiveVsDraft {
        live: if is_ext(draft) && !is_ext(live) {
            "not offered yet".to_owned()
        } else {
            shown(live, template_label)
        },
        draft: shown(draft, template_label),
    };
    (out.live != out.draft).then_some(out)
}

fn pin_tag(bases: &Bases, prefix: &str) -> Option<String> {
    let nested = format!("{prefix}/");
    bases
        .pins
        .iter()
        .find(|p| p.layer == prefix || p.layer.starts_with(&nested))
        .map(|p| format!("{}@v{}", p.layer, p.version))
}

/// Where the activity comes from: the template pins (+ the department archetype and the region) → the tenant.
fn lineage(
    bases: &Bases,
    a: &JobJourneyActivity,
    stage: &JourneyStage,
    operations: &[ActivityOperation],
    card_refined: bool,
) -> Vec<UseCaseLayer> {
    let ext = a.refinement == RefinementState::Extension;
    let objs: Vec<&str> = a
        .activity
        .objects
        .iter()
        .map(|o| object_key(&o.object))
        .collect();
    let mut distinct: Vec<&str> = Vec::new();
    for &o in &objs {
        if !distinct.contains(&o) {
            distinct.push(o);
        }
    }
    let required = operations.iter().filter(|o| o.required).count();
    let not_in_template = "nothing \u{2014} this activity is not in the template";
    let mut layers = vec![
        UseCaseLayer {
            kind: "base",
            tag: pin_tag(bases, "base"),
            contributes: format!(
                "the {} object{}, their operations and state machines",
                if distinct.is_empty() {
                    "canonical".to_owned()
                } else {
                    distinct.join(", ")
                },
                if objs.len() == 1 { "" } else { "s" }
            ),
            card: None,
        },
        UseCaseLayer {
            kind: "industry",
            tag: pin_tag(bases, "industry"),
            contributes: format!("the {} stage and its customer themes", stage.label),
            card: None,
        },
    ];
    if let Some(r) = bases.selection.region.as_deref().filter(|r| !r.is_empty()) {
        layers.push(UseCaseLayer {
            kind: "industry",
            tag: pin_tag(bases, "industry"),
            contributes: format!("the {r} region variants"),
            card: None,
        });
    }
    for p in bases
        .pins
        .iter()
        .filter(|p| p.layer.starts_with("base_dept/"))
    {
        layers.push(UseCaseLayer {
            kind: "department",
            tag: Some(format!("{}@v{}", p.layer, p.version)),
            contributes: "the activities the department archetype shares".to_owned(),
            card: None,
        });
    }
    layers.push(UseCaseLayer {
        kind: "department",
        tag: pin_tag(bases, "industry_dept"),
        contributes: if ext {
            not_in_template.to_owned()
        } else {
            "this activity: trigger, objects, field subset, knowledge kinds".to_owned()
        },
        card: None,
    });
    layers.push(UseCaseLayer {
        kind: "job",
        tag: pin_tag(bases, "job"),
        contributes: if ext {
            not_in_template.to_owned()
        } else {
            format!(
                "which operations are required ({required} R, {} O) and the north-star metric",
                operations.len() - required
            )
        },
        card: None,
    });
    layers.push(UseCaseLayer {
        kind: "tenant",
        tag: None,
        contributes: match a.refinement {
            RefinementState::Extension => "you added this activity (tenant extension)".to_owned(),
            RefinementState::Hidden => {
                "you hid this activity: customers are not offered it".to_owned()
            }
            RefinementState::Relabelled => format!(
                "you renamed it from \u{201c}{}\u{201d}",
                a.base_label.as_deref().unwrap_or_default()
            ),
            RefinementState::Base if card_refined => {
                "you refined the use case card; the rest is the template's".to_owned()
            }
            RefinementState::Base => "no refinement: used as the template defines it".to_owned(),
        },
        card: Some(if card_refined { "refined" } else { "template" }),
    });
    layers
        .into_iter()
        .filter(|l| l.kind == "tenant" || l.tag.is_some())
        .collect()
}

/// The use case of activity `key` of the job's journey `of` the draft, the live release or a release (module docs).
/// 404 `journey_not_found` (see [`job_journey`]), 404 `activity_not_found`. The route answers this (web
/// `ActivityDetail`); the use case proper (web `ActivityUseCase`) is its `use_case`.
#[allow(clippy::too_many_lines)]
pub async fn job_activity(
    c: &mut PgConnection,
    st: &AppState,
    job: &JobRef,
    key: &str,
    of: &DesignOf,
) -> ApiResult<ActivityDetail> {
    let b = build(&mut *c, st, job, of).await?;
    let j = &b.journey;
    let a = j
        .activities
        .iter()
        .find(|a| a.activity.key == key)
        .cloned()
        .ok_or_else(|| {
            ApiError::Missing(
                "activity_not_found",
                format!("This job has no activity {key}."),
            )
        })?;
    let stage = j
        .stages
        .iter()
        .find(|s| s.key == a.activity.stage)
        .cloned()
        .unwrap_or_else(|| JourneyStage {
            key: a.activity.stage.clone(),
            label: a.activity.stage.as_str().to_owned(),
            order: 0,
            description: String::new(),
            synonyms: Vec::new(),
            out_of_scope: false,
            themes: Vec::new(),
            layer: None,
        });
    let slug = key.split_once(':').map_or(key, |(_, s)| s);

    // operations: the catalogue attributes and the routes per env
    let cat = deps::catalogue(&b.bases);
    let operations: Vec<ActivityOperation> = a
        .activity
        .operations
        .iter()
        .map(|o| {
            let m = deps::op_meta(&cat, &o.op, o.variant.as_deref());
            ActivityOperation {
                op: o.op.clone(),
                variant: o.variant.clone(),
                required: o.required,
                modality: m.modality,
                side_effect: m.side_effect,
                identity_level: m.identity_level,
                routes: deps::routes_for(&b.routes, &o.op, o.variant.as_deref()),
            }
        })
        .collect();
    let unrouted_required: Vec<UnroutedOp> = operations
        .iter()
        .filter(|o| o.required)
        .flat_map(|o| {
            o.routes
                .iter()
                .filter(|r| r.kind == "unrouted")
                .map(|r| UnroutedOp {
                    op: op_label(&o.op, o.variant.as_deref()),
                    env: r.env.clone(),
                })
        })
        .collect();

    // the context space and its procedure (R9)
    let space = deps::context_space(&b.bases, &b.industry, key, a.activity.area.as_deref());
    let live7 = deps::runs_7d(&mut *c, job, key).await?;
    let procedure = match &space {
        Some(sp) => {
            deps::procedure_for(&mut *c, &st.design, job, of, &b.bases, sp, &b.routes, live7)
                .await?
        }
        None => None,
    };

    // the gates on the way in (R9)
    let modality = if operations.iter().any(|o| o.modality == "do") {
        "do"
    } else if operations.iter().any(|o| o.modality == "request") {
        "request"
    } else {
        "observe"
    };
    let subject = a
        .activity
        .objects
        .iter()
        .find(|o| o.role == ObjectRole::Subject)
        .or_else(|| a.activity.objects.first())
        .map(|o| object_key(&o.object).to_owned());
    let slots: Vec<String> = a
        .activity
        .objects
        .iter()
        .flat_map(|o| o.required_inputs.iter().cloned())
        .collect();
    let gate_stats = stats::read(&mut *c, job.id, Utc::now()).await?;
    let gates = deps::use_case_gates(
        &b.bases,
        &gate_stats,
        &GateSubject {
            key,
            label: &a.activity.label,
            stage: &stage,
            object: subject.as_deref(),
            modality,
            slots: &slots,
        },
    );

    // identity: what the procedure runs, else what the activity's operations ask
    let levels: Vec<u8> = match &procedure {
        Some(p) => p
            .steps
            .iter()
            .map(|s| level_of(&s.identity_level))
            .collect(),
        None => operations
            .iter()
            .map(|o| level_of(&o.identity_level))
            .collect(),
    };
    let level = levels.into_iter().max().unwrap_or(0).min(3);
    let identity = IDENTITY[usize::from(level)].clone();

    // state changes: the write operations (+ the procedure's write steps) over the compile's object transitions
    let mut writes: Vec<String> = Vec::new();
    for o in operations.iter().filter(|o| o.side_effect != "R") {
        if !writes.contains(&o.op) {
            writes.push(o.op.clone());
        }
    }
    if let Some(p) = &procedure {
        for s in p.steps.iter().filter(|s| s.side_effect != "R") {
            let op = s.op.split('{').next().unwrap_or(&s.op).to_owned();
            if !writes.contains(&op) {
                writes.push(op);
            }
        }
    }
    let objects: Vec<CatalogueItem> = if writes.is_empty() {
        Vec::new()
    } else {
        match templates::compile_pinned(
            &mut *c,
            &b.bases.pins,
            &b.bases.selection,
            job_key(&b.bases.pins),
        )
        .await?
        {
            Ok(compiled) => compiled.catalogue.objects,
            Err(_) => Vec::new(),
        }
    };
    let state_changes = state_changes(&writes, &objects);

    // the knowledge kinds of the stage (+ every-stage ones)
    let kinds: Vec<JobKnowledgeKind> = j
        .knowledge_kinds
        .iter()
        .filter(|k| k.base.stage == a.activity.stage.as_str() || k.base.stage == ANY_STAGE)
        .cloned()
        .collect();

    // checks on the reply
    let mut checks = Vec::new();
    let reads: Vec<String> = operations
        .iter()
        .filter(|o| o.side_effect == "R" && o.op != "knowledge.search")
        .map(|o| op_label(&o.op, o.variant.as_deref()))
        .collect();
    if !reads.is_empty() {
        checks.push(UseCaseCheck {
            key: "F1",
            text: format!(
                "Every fact in the reply must match what {} returned.",
                reads.join(" / ")
            ),
        });
    }
    if !writes.is_empty() {
        checks.push(UseCaseCheck {
            key: "F3",
            text: format!(
                "The reply may only claim a change ({}) when that operation returned ok.",
                writes.join(", ")
            ),
        });
    }
    if !kinds.is_empty() {
        checks.push(UseCaseCheck {
            key: "F2",
            text: "Policy statements cite a passage from the knowledge store.".to_owned(),
        });
    }
    if level >= 2 {
        checks.push(UseCaseCheck {
            key: "F5",
            text: "No personal data beyond the verified customer.".to_owned(),
        });
    }

    // the card: the pinned stack's, the tenant's `card` over it
    let mut card: Card = deps::pinned_card(&mut *c, &b.bases.pins, &b.bases.selection, key)
        .await?
        .unwrap_or_default();
    let mut card_refined = false;
    if let Some(r) = b.rows.get(key)
        && r.kind != TOMBSTONE
        && let Some(own) = r
            .item
            .as_ref()
            .and_then(|i| i.get("card"))
            .and_then(Value::as_object)
    {
        card_refined = !own.is_empty();
        card.overlay(own);
    }
    let chat: Vec<String> = j
        .messages
        .iter()
        .find(|m| m.stage == a.activity.stage && m.channel == ChannelKey::Chat)
        .map(|m| m.utterances.clone())
        .unwrap_or_default();
    let examples = card.examples.clone().unwrap_or_else(|| {
        let trigger = a
            .activity
            .trigger
            .trim_start_matches('\u{201c}')
            .trim_end_matches('\u{201d}')
            .to_owned();
        (!trigger.is_empty())
            .then_some(trigger)
            .into_iter()
            .chain(chat.iter().take(2).cloned())
            .collect()
    });
    let summary = card
        .summary
        .clone()
        .unwrap_or_else(|| summary_of(&a.activity, &operations, !writes.is_empty()));

    // live signals (B4)
    let live30 = signals::use_case_live(&mut *c, job, key, OUTCOME_DAYS).await?;
    let recent_cases = signals::recent_for_activity(&mut *c, job, key, RECENT_CASES).await?;

    // D62: live vs draft
    let live_vs_draft = if *of == DesignOf::Draft {
        let live = overlay_rows(&mut *c, st, job, &DesignOf::Live).await?;
        live_vs_draft(
            live.get(key),
            b.rows.get(key),
            a.base_label.as_deref().unwrap_or(&a.activity.label),
        )
    } else {
        None
    };
    let draft_rev = design::draft_rev(&mut *c, job).await?;

    let use_case = ActivityUseCase {
        summary,
        identity,
        examples,
        gates,
        procedure,
        state_changes,
        checks,
        live: live30.live,
        outcomes: live30.outcomes,
        recent_cases,
        lineage: lineage(&b.bases, &a, &stage, &operations, card_refined),
        pending: j
            .pending
            .iter()
            .filter(|p| p.activity.as_deref() == Some(key))
            .cloned()
            .collect(),
        live_vs_draft,
        draft_rev,
    };
    Ok(ActivityDetail {
        sample_dialogue: deps::dialogue(card.sample_dialogue.as_deref().unwrap_or_default()),
        success_example: card.success_example.clone().unwrap_or_else(deps::no_example),
        error_example: card.error_example.clone().unwrap_or_else(deps::no_example),
        metric: card.metric.clone().unwrap_or_else(|| {
            json!({"key": slug, "label": a.activity.label, "target": "\u{2014}", "current": null})
        }),
        context_space: space,
        procedure: format!("proc.{slug}"),
        knowledge_kinds: kinds,
        unrouted_required,
        use_case,
        operations,
        stage,
        activity: a,
    })
}
