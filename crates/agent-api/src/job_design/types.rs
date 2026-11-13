//! B5 wire types: the shapes of `web/src/lib/api/types/ontology.ts` that the job-design routes serve (J11 ontology,
//! J12 context spaces + procedures, J7 gates, J8 toolsets), field for field, snake_case.
//!
//! Serde rules (checked against the fixture world by `tests/b5/types.rs`):
//! - Inputs (`*Input`, [`GateOptionRefinement`], [`ProcedureStep`]) are `deny_unknown_fields`: an unknown or extra
//!   field is 422 `invalid_body`.
//! - `T | null` is an `Option<T>` that encodes as `null`, never as an absent field.
//! - A TS-optional field (`x?: T`) that the server can always compute is a plain field with `#[serde(default)]` (it
//!   decodes from a payload without it and is always sent). Only where the web gives *absence* a meaning is it
//!   skipped when unset: `ContextSpace.live_required` ("absent = same as `required`") and `GateStats.confidence`
//!   ("absent without traffic").
//! - Procedure `version` is an integer (B5 R6: `agent_case.procedure_version integer`); T7 moves the web type.
use std::collections::BTreeMap;

use agent_model::kinds::catalog::{Modality, SideEffect};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize};

use crate::runtime::Env;
pub use crate::runtime::conditions::{CondOp, Condition};

/// D1: one page of a list, `next` the cursor of the next (older) page, `null` at the end.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Paged<T> {
    pub items: Vec<T>,
    pub next: Option<String>,
}

/// A field that may be absent (keep) or null (revert): `Some(None)` = null, `None` = absent.
fn present<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}

/// Where a row comes from: the pinned template layers (`base`) or this tenant's refinements (`tenant`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    Base,
    Tenant,
}

// ---- J11 ontology ---------------------------------------------------------------------------------------------

/// The template layer an entity is authored in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OntologyScope {
    Generic,
    Industry,
    IndustryDept,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Cardinality {
    #[serde(rename = "1:1")]
    OneToOne,
    #[serde(rename = "1:n")]
    OneToMany,
    #[serde(rename = "n:1")]
    ManyToOne,
    #[serde(rename = "n:m")]
    ManyToMany,
}

/// A refinement whose base parent is gone (05 §4.6).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttributeConflict {
    ParentMissing,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OntologyAttribute {
    pub name: String,
    pub data_type: String,
    pub pii: bool,
    pub synonyms: Vec<String>,
    pub origin: Origin,
    #[serde(default)]
    pub conflict: Option<AttributeConflict>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OntologyEntity {
    pub tenant_id: String,
    pub job: String,
    /// PascalCase key (`Order`).
    pub name: String,
    pub label: String,
    pub scope: OntologyScope,
    pub industry: Option<String>,
    pub department: Option<String>,
    pub synonyms: Vec<String>,
    pub attributes: Vec<OntologyAttribute>,
    pub origin: Origin,
    pub hidden: bool,
    pub object_key: Option<String>,
    /// Layer tag it was authored in (`job/customer_care_agent@v3`) or `refinement r14`.
    pub layer: String,
    pub created_by: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OntologyRelation {
    pub tenant_id: String,
    pub job: String,
    pub subject: String,
    pub predicate: String,
    pub object: String,
    pub cardinality: Cardinality,
    pub origin: Origin,
}

/// An attribute as `createOntologyEntity` / `addOntologyAttribute` take it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttributeInput {
    pub name: String,
    pub data_type: String,
    pub pii: bool,
    pub synonyms: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntityInput {
    pub name: String,
    pub label: String,
    pub scope: OntologyScope,
    pub industry: Option<String>,
    pub department: Option<String>,
    pub synonyms: Vec<String>,
    pub object_key: Option<String>,
    pub attributes: Vec<AttributeInput>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelationInput {
    pub subject: String,
    pub predicate: String,
    pub object: String,
    pub cardinality: Cardinality,
}

// ---- J12 context spaces + procedures ----------------------------------------------------------------------------

/// A space the base removed or changed under a tenant row (05 §4.6).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpaceConflict {
    BaseRemoved,
    BaseChanged,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ContextSpace {
    pub tenant_id: String,
    pub job: String,
    /// D61 key `<industry>:<area>:<slug>` (`crate::job_design::keys`).
    pub key: String,
    pub label: String,
    pub stage: String,
    pub activity: String,
    pub request_type: String,
    pub entities: Vec<String>,
    /// Effective required slots (after the tenant override).
    pub required: Vec<String>,
    pub base_required: Vec<String>,
    /// The slots production runs with; absent = same as `required`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_required: Option<Vec<String>>,
    pub origin: Origin,
    pub overridden: bool,
    pub conflict: Option<SpaceConflict>,
    /// Procedure name the orchestrator runs for this space.
    pub procedure: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum VersionStatus {
    Draft,
    Canary,
    Published,
    Archived,
    Discarded,
}

/// One step of a procedure version (also the shape `createProcedureVersion` takes).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcedureStep {
    pub step_number: u32,
    /// 0-based phase; steps in one phase run in parallel.
    pub phase: u32,
    pub name: String,
    /// `object.verb`
    pub operation: String,
    /// Variant without braces, or null.
    pub variant: Option<String>,
    pub modality: Modality,
    pub side_effect: SideEffect,
    /// As the web shows it (`2`, `1→2`, `S`): a display value, not a parsed level.
    pub identity_level: String,
    pub confirm_back: bool,
    /// The case ends as not eligible when this step's outcome is false.
    pub gate: bool,
    /// Slot bindings `arg ← $slot`.
    pub args: BTreeMap<String, String>,
    /// T7a (F-20): checked by the runner before the call; facts of steps in EARLIER phases only.
    #[serde(default)]
    pub preconditions: Vec<Condition>,
    /// T7a (F-20): checked after an `ok` answer; may read this step's own output too.
    #[serde(default)]
    pub postconditions: Vec<Condition>,
    /// T7a: the job's agent role that may run the step (`catalogue.agent_roles`); null = the procedure role.
    #[serde(default)]
    pub performed_by: Option<String>,
    /// T7a (F-15): the per-case cap of an X/P step, 1..=20; null = B4's default (1). Never set on R/W steps.
    #[serde(default)]
    pub max_per_case: Option<u32>,
}

/// The one check every writer of a platform `procedure` row shares (the seeder's delta validation and
/// `platform::db::rows::write_revision`): when `fields.steps` is a list, every entry must deserialize as a
/// [`ProcedureStep`] - `deny_unknown_fields`, with `operation`, `modality`, `side_effect` and `identity_level`
/// required. The runtime (`TemplateProcedure::from_item`) reads the steps of the row; an entry that does not decode
/// would be dropped there without a word, and a gate step such as `rule.evaluate{RET-01}` would never run. A row
/// without `steps`, or whose `steps` is a bare count (the catalogue's `CountOrList` shape), has no step bodies to
/// decode and passes.
pub fn procedure_steps_decode(fields: &serde_json::Value) -> Result<(), String> {
    let Some(steps) = fields.get("steps").and_then(|s| s.as_array()) else {
        return Ok(());
    };
    let problems: Vec<String> = steps
        .iter()
        .enumerate()
        .filter_map(|(i, step)| {
            serde_json::from_value::<ProcedureStep>(step.clone())
                .err()
                .map(|e| format!("steps[{i}] is not a ProcedureStep: {e}"))
        })
        .collect();
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems.join("; "))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryAction {
    Create,
    Approve,
    Transition,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProcedureHistoryEntry {
    pub at: DateTime<Utc>,
    pub actor: String,
    pub action: HistoryAction,
    pub from: Option<VersionStatus>,
    pub to: Option<VersionStatus>,
    pub ok: bool,
    pub reason: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TestRunOutcome {
    Resolved,
    NotEligible,
    HandedOver,
    Refused,
}

/// One staff test chat against a procedure version (env `test`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProcedureTestRun {
    pub at: DateTime<Utc>,
    pub actor: String,
    pub utterance: String,
    pub outcome: TestRunOutcome,
    /// Last step number that ran (0 = none).
    pub reached_step: u32,
    pub note: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProcedureVersion {
    pub tenant_id: String,
    pub job: String,
    pub name: String,
    /// Integer (R6).
    pub version: u32,
    pub status: VersionStatus,
    pub origin: Origin,
    pub context_space: String,
    pub description: String,
    pub reviewers: Vec<String>,
    pub steps: Vec<ProcedureStep>,
    pub history: Vec<ProcedureHistoryEntry>,
    pub updated_at: DateTime<Utc>,
    pub runs_7d: u64,
    /// (completed + not_eligible) / runs_7d; null without runs.
    pub success_rate: Option<f64>,
    /// The job release this version was in at its last cut/apply, or null.
    #[serde(default)]
    pub released_in: Option<String>,
    #[serde(default)]
    pub test_runs: Vec<ProcedureTestRun>,
    /// T7a (F-20): checked when the last phase finished; empty = none.
    #[serde(default)]
    pub success_criteria: Vec<Condition>,
    /// T7a: operations (`object.verb{variant}`) the procedure needs routed before its first step.
    #[serde(default)]
    pub tool_requirements: Vec<String>,
    /// T7a: a PDDL domain, stored verbatim and never interpreted (≤ 64 KiB).
    #[serde(default)]
    pub pddl_domain: Option<String>,
}

/// A version row with the effective flags computed (list and detail pages).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProcedureView {
    #[serde(flatten)]
    pub version: ProcedureVersion,
    /// A base PUBLISHED version that a tenant PUBLISHED version of the same name shadows.
    pub shadowed: bool,
    /// What production runs (the live release).
    pub effective: bool,
    /// What the Test environment runs (the draft).
    #[serde(default)]
    pub effective_draft: bool,
    /// Status in the live design; null = not in production.
    #[serde(default)]
    pub live_status: Option<VersionStatus>,
    /// Steps whose operation is outside the derived grant of the space's activity (12 §2.1).
    pub outside_grant: Vec<String>,
    /// The activity grant the steps are checked against (`object.verb{variant}`).
    pub activity_grant: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcedureVersionInput {
    pub name: String,
    pub version: u32,
    pub context_space: String,
    pub description: String,
    pub steps: Vec<ProcedureStep>,
    /// T7a: version-level success criteria (may read every step).
    #[serde(default)]
    pub success_criteria: Vec<Condition>,
    /// T7a: catalogue operations the procedure needs.
    #[serde(default)]
    pub tool_requirements: Vec<String>,
    /// T7a: stored verbatim, never parsed.
    #[serde(default)]
    pub pddl_domain: Option<String>,
}

// ---- J7 gates ---------------------------------------------------------------------------------------------------

/// Where in a turn a gate is asked: P1 the request, P2 before a tool call, R1 the draft response.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum GatePoint {
    P1,
    P2,
    R1,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateType {
    Noul,
    Choice,
    Score,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateFail {
    Open,
    Closed,
}

/// Library grouping; `tenant` = a question a tenant added to one job.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateGroup {
    Base,
    Safety,
    Post,
    Billing,
    Itsd,
    Utilities,
    Tenant,
}

/// The playground's name for a gate point (`GateEvalInput.point`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateRunPoint {
    Request,
    Tool,
    Response,
}

impl GateRunPoint {
    /// request → P1, tool → P2, response → R1.
    pub fn gate_point(self) -> GatePoint {
        match self {
            GateRunPoint::Request => GatePoint::P1,
            GateRunPoint::Tool => GatePoint::P2,
            GateRunPoint::Response => GatePoint::R1,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateFireWhen {
    Gte,
    Lt,
    In,
}

/// What a fired question does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateAction {
    Handover,
    Block,
    Annotate,
}

/// What a gate point decided for the turn. Ordered by strength: allow < block < handover.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateDecision {
    Allow,
    Block,
    Handover,
}

/// The structured firing rule (web `GateRule`, `lib/ontology/gateRules.ts`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GateRule {
    pub fire_when: GateFireWhen,
    /// gte / lt: the line (0–1 probability, or a score level). null with `in`.
    pub threshold: Option<f64>,
    /// in: the option keys that fire. null with gte / lt.
    pub fire_values: Option<Vec<String>>,
    /// Below this confidence the answer is "unsure". null = any confidence.
    pub min_confidence: Option<f64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuestionOrigin {
    Platform,
    Tenant,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EffGateOption {
    pub key: String,
    pub label: String,
    pub description: String,
    /// Produced at the release cut from the job's effective grid; read-only.
    pub generated: bool,
    pub base_label: String,
    pub hidden: bool,
    pub refined: bool,
    /// Verdicts with this option as top answer, last 7 days.
    pub verdicts: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GateStats {
    pub verdicts_7d: u64,
    pub fail_open: u64,
    pub fail_closed: u64,
    pub below_threshold: u64,
    pub latency_p50_ms: Option<u64>,
    pub latency_p95_ms: Option<u64>,
    pub timeouts: u64,
    /// Top-answer confidence of choice gates in 20 buckets of 0.05; absent without traffic.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<Vec<u64>>,
}

/// One question as J7 shows it: the library (or tenant) question with this job's refinements applied.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EffGateQuestion {
    pub key: String,
    pub group: GateGroup,
    /// Template layer (`base`, `industry/utilities`, `job/it_service_desk_agent`).
    pub layer: String,
    pub point: GatePoint,
    #[serde(rename = "type")]
    pub kind: GateType,
    pub question: String,
    pub options: Vec<EffGateOption>,
    pub generated_from: Option<String>,
    /// Threshold as written (`top ≥ 0.6`, `any true`).
    pub threshold: String,
    /// Numeric part a tenant may tune; null = not tunable.
    pub threshold_value: Option<f64>,
    pub fail: GateFail,
    /// Safety gate: fail closed, never hideable (D29).
    pub safety: bool,
    pub effect: String,
    pub rule: GateRule,
    pub action: GateAction,
    /// Tool point only: the tools it is asked for; null / empty = every tool.
    pub tools: Option<Vec<String>>,
    pub origin: QuestionOrigin,
    pub updated_by: Option<String>,
    pub updated_at: DateTime<Utc>,
    pub base_threshold_value: Option<f64>,
    pub threshold_refined: bool,
    pub base_rule: GateRule,
    pub rule_refined: bool,
    pub stats: GateStats,
    /// F-17 (T8a): asked in this job (a safety question is always on).
    #[serde(default = "yes")]
    pub enabled: bool,
    /// F-16 (T8a): the text the customer reads when this question hands over or blocks: the job's setting, else
    /// [`customer_reply_default`](Self::customer_reply_default); null = the care-bus default.
    #[serde(default)]
    pub customer_reply: Option<String>,
    /// The question's own text (library `customer_reply` field, a tenant question's column); null = none.
    #[serde(default)]
    pub customer_reply_default: Option<String>,
}

fn yes() -> bool {
    true
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateChangeField {
    Label,
    Description,
    Hidden,
    Threshold,
    Rule,
    Question,
}

/// Where a refinement stands (D54): applied release, approved release, or the draft (next cut).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateChangeShips {
    Live,
    Approved,
    NextCut,
}

/// One tenant refinement of a gate, field by field.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GateChange {
    pub gate: String,
    /// null = question-level.
    pub option: Option<String>,
    pub field: GateChangeField,
    pub from: String,
    pub to: String,
    pub by: String,
    pub by_name: String,
    pub at: DateTime<Utc>,
    pub ships: GateChangeShips,
    pub release_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LiveRelease {
    pub id: String,
    pub released_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ApprovedRelease {
    pub id: String,
    pub approved_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct JobGateReleases {
    pub live: Option<LiveRelease>,
    pub approved: Option<ApprovedRelease>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct JobGates {
    pub questions: Vec<EffGateQuestion>,
    /// Since when the stats count, or null without traffic.
    pub window_from: Option<DateTime<Utc>>,
    /// This job's tenant refinements of its gates, newest first.
    #[serde(default)]
    pub changes: Vec<GateChange>,
    #[serde(default)]
    pub releases: JobGateReleases,
}

/// `refineGateOption`: an absent field keeps the refinement, null (or blank) reverts it to the base.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GateOptionRefinement {
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub label: Option<Option<String>>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub description: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hidden: Option<bool>,
}

/// The true / false criteria Laya answers a tenant (noul) question against.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NoulCriteria {
    #[serde(rename = "true")]
    pub yes: String,
    #[serde(rename = "false")]
    pub no: String,
}

/// `addGateQuestion`: a tenant question for one job (noul only).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GateQuestionInput {
    pub key: String,
    pub point: GatePoint,
    pub question: String,
    pub criteria: NoulCriteria,
    pub rule: GateRule,
    pub action: GateAction,
    pub fail: GateFail,
    #[serde(default)]
    pub tools: Option<Vec<String>>,
    /// F-16 (T8a): the question's own customer text (1–500 characters); absent / null = the care-bus default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub customer_reply: Option<String>,
}

/// Which design an evaluation runs (D54).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Against {
    Live,
    Draft,
}

/// `evaluateGates`: try a message against one gate point.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GateEvalInput {
    pub point: GateRunPoint,
    pub text: String,
    /// Tool point only: the tool the worker would call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// Default: the draft when it differs from live, else live.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub against: Option<Against>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AnswerProbability {
    pub key: String,
    pub label: String,
    pub p: f64,
}

/// One question's answer in an evaluation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GateEvalAnswer {
    pub gate: String,
    pub question: String,
    #[serde(rename = "type")]
    pub kind: GateType,
    /// The answer as a label (`true`, `Issue`, `4 · High`).
    pub answer: String,
    /// Option key of the answer (`true`, `issue`, `4`).
    pub value: String,
    /// Per-option probability, highest first.
    pub probabilities: Vec<AnswerProbability>,
    pub confidence: f64,
    pub rule: GateRule,
    pub fail: GateFail,
    pub safety: bool,
    pub fired: bool,
    pub unsure: bool,
    /// fired → its action; unsure + fail closed → handover; else null.
    pub action: Option<GateAction>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SkippedQuestion {
    pub gate: String,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GateEvaluation {
    pub point: GateRunPoint,
    pub gate_point: GatePoint,
    pub ran_against: Against,
    pub draft_differs: bool,
    pub release_id: Option<String>,
    pub decision: GateDecision,
    /// Gate keys that decided (fired with handover / block, or failed closed).
    pub deciding: Vec<String>,
    pub answers: Vec<GateEvalAnswer>,
    pub skipped: Vec<SkippedQuestion>,
    pub latency_ms: u64,
}

/// One question's verdict at one gate point of a case turn.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GateVerdictRow {
    pub id: String,
    pub at: DateTime<Utc>,
    pub point: GatePoint,
    pub gate: String,
    pub question: String,
    pub answer: String,
    pub confidence: f64,
    pub below_threshold: bool,
    /// What the gate point decided for that turn.
    pub action: GateDecision,
    pub case_id: String,
    pub conversation_id: String,
}

// ---- J8 toolsets ------------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentRoleKey {
    Qualifier,
    Procedure,
    PostGate,
    Assistant,
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentRoleGrant {
    pub object: String,
    pub verb: String,
    pub variant: Option<String>,
    pub modalities: Vec<Modality>,
    /// Procedure role: the grant is the activity's operations, never hand-authored.
    pub derive_from_activity: bool,
    /// D119: derived from the job's operation expectations, not a stored grant.
    #[serde(default, skip_serializing_if = "is_false")]
    pub derived: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentRole {
    /// Job template the role belongs to.
    pub job: String,
    pub role: AgentRoleKey,
    pub label: String,
    pub used_by: String,
    pub max_side_effect: SideEffect,
    pub enabled: bool,
    pub grants: Vec<AgentRoleGrant>,
    pub note: String,
    pub updated_by: Option<String>,
    pub updated_at: DateTime<Utc>,
}

/// One operation of the catalogue as the ontology pages show it (N1).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CatalogueOperation {
    /// `object.verb`
    pub op: String,
    pub variant: Option<String>,
    pub modality: Modality,
    pub side_effect: SideEffect,
    /// `0`, `1`, `2`, `3`, `S` (the web shows ranges such as `1→2` in its concept list).
    pub identity_level: String,
}

/// N4 as J8 shows it (web `RouteTarget` of `types/ontology.ts`, not the connectors' one).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolsetRoute {
    Instance {
        instance: String,
    },
    Builtin,
    /// Always `agent_cases`.
    Fallback {
        instance: String,
    },
    DeepLink {
        url: String,
    },
    NotOffered,
    Unrouted,
}

/// N2: the activity expects an op, R(equired) or O(ptional).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Expected {
    R,
    O,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolsetRow {
    pub op: String,
    pub variant: Option<String>,
    pub side_effect: SideEffect,
    pub modality: Modality,
    /// null = the role grants it but the activity does not.
    pub expected: Option<Expected>,
    /// N3, before the tenant narrowing.
    pub granted: bool,
    pub narrowed: bool,
    pub route: ToolsetRoute,
    /// N2 ∩ N3 ∩ N4.
    pub effective: bool,
    /// Why not, when not effective.
    pub reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ActivityRef {
    pub key: String,
    pub stage: String,
    pub label: String,
    pub hidden: bool,
    pub origin: Origin,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Toolset {
    pub role: AgentRoleKey,
    pub role_enabled: bool,
    pub activity: String,
    pub env: Env,
    pub rows: Vec<ToolsetRow>,
}

/// J8 `toolsetMeta` role (T11a, F-40): the role at the design's pins plus the job's switch. `enabled` is the
/// effective switch (`enabled_job` ∧ `enabled_platform`). The server always sends the three switches; they are
/// `Option` only so a payload from before T11a (the fixture world) still decodes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolsetRole {
    #[serde(flatten)]
    pub role: AgentRole,
    /// The job's switch (`role_setting`); a role a job cannot configure is always `true` here.
    #[serde(default)]
    pub enabled_job: Option<bool>,
    /// The platform `agent_role.enabled` at the design's pins.
    #[serde(default)]
    pub enabled_platform: Option<bool>,
    /// The job's tool calls per assistant turn; null = the runtime default (6).
    #[serde(default)]
    pub max_tool_calls: Option<u32>,
    /// A5: seconds per assistant turn (5..=600); null = the runtime default (60).
    #[serde(default)]
    pub turn_budget_s: Option<u32>,
    /// A5: the job's own assistant task prompt; null = the audience's default.
    #[serde(default)]
    pub system_prompt: Option<String>,
    /// A5: a turn on a conversation with an open case continues that case.
    #[serde(default)]
    pub case_per_thread: bool,
    /// May the job switch this role (only `assistant` today)?
    #[serde(default)]
    pub configurable: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolsetMeta {
    pub roles: Vec<ToolsetRole>,
    pub activities: Vec<ActivityRef>,
    /// `role|activity|op{variant}`.
    pub narrowings: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolsetProcedureStep {
    pub step_number: u32,
    pub name: String,
    pub operation: String,
    pub variant: Option<String>,
    pub side_effect: SideEffect,
    pub identity_level: String,
    pub confirm_back: bool,
}

/// The procedure the orchestrator runs (as role `procedure`) for one activity.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolsetActivityProcedure {
    pub activity: String,
    pub context_space: String,
    pub name: String,
    /// Integer (R6).
    pub version: u32,
    pub origin: Origin,
    pub steps: Vec<ToolsetProcedureStep>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolsetNarrowing {
    pub role: AgentRoleKey,
    pub activity: String,
    /// `object.verb{variant}`
    pub op: String,
    pub by: String,
    pub at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolsetOverview {
    pub env: Env,
    pub toolsets: Vec<Toolset>,
    pub procedures: Vec<ToolsetActivityProcedure>,
    pub narrowings: Vec<ToolsetNarrowing>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn option_refinement_tells_absent_from_null() {
        let r: GateOptionRefinement = serde_json::from_value(json!({"label": null})).unwrap();
        assert_eq!(r.label, Some(None));
        assert_eq!(r.description, None);
        let r: GateOptionRefinement =
            serde_json::from_value(json!({"description": "x", "hidden": true})).unwrap();
        assert_eq!(r.description, Some(Some("x".into())));
        assert_eq!(r.hidden, Some(true));
        assert!(serde_json::from_value::<GateOptionRefinement>(json!({"lable": "x"})).is_err());
    }

    #[test]
    fn run_points_map_to_gate_points() {
        assert_eq!(GateRunPoint::Request.gate_point(), GatePoint::P1);
        assert_eq!(GateRunPoint::Tool.gate_point(), GatePoint::P2);
        assert_eq!(GateRunPoint::Response.gate_point(), GatePoint::R1);
        assert!(GateDecision::Handover > GateDecision::Block);
        assert!(GateDecision::Block > GateDecision::Allow);
    }

    fn step_json() -> serde_json::Value {
        json!({
            "step_number": 1, "phase": 0, "name": "Read the order", "operation": "order.read",
            "variant": null, "modality": "observe", "side_effect": "R", "identity_level": "2",
            "confirm_back": false, "gate": false, "args": {"order_id": "$order_id"}
        })
    }

    #[test]
    fn procedure_steps_decode_accepts_valid_steps() {
        assert_eq!(
            procedure_steps_decode(&json!({"label": "x", "steps": [step_json()]})),
            Ok(())
        );
        assert_eq!(procedure_steps_decode(&json!({"label": "x"})), Ok(()));
        assert_eq!(procedure_steps_decode(&json!({"steps": []})), Ok(()));
        // the catalogue's bare count has no step bodies
        assert_eq!(procedure_steps_decode(&json!({"steps": 4})), Ok(()));
    }

    #[test]
    fn procedure_steps_decode_refuses_a_note() {
        let mut gate = step_json();
        gate["operation"] = json!("rule.evaluate");
        gate["note"] = json!("builtin");
        let err = procedure_steps_decode(&json!({"steps": [step_json(), gate]})).unwrap_err();
        assert!(err.starts_with("steps[1]") && err.contains("note"), "{err}");
    }

    #[test]
    fn procedure_steps_decode_refuses_a_null_operation() {
        let mut talk = step_json();
        for k in ["operation", "modality", "side_effect", "identity_level"] {
            talk[k] = serde_json::Value::Null;
        }
        let err = procedure_steps_decode(&json!({"steps": [talk]})).unwrap_err();
        assert!(err.starts_with("steps[0]"), "{err}");
        let mut only_op = step_json();
        only_op["operation"] = serde_json::Value::Null;
        assert!(procedure_steps_decode(&json!({"steps": [only_op]})).is_err());
    }
}
