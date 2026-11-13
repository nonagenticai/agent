//! The layer compiler (plan B6 §6, B6-6/B6-7): a pure function from a D65 selection and already-loaded layer
//! rows to the UI `EffectiveConfig` plus the folded catalogue. Ported from strata_configurator's
//! `configurator-core/src/compile.rs` (pin 71aa7af) and re-cut to the web `compileModel`
//! (`web/src/lib/platform/model.ts`), which is the golden reference:
//!
//! - layer order: THE fold order of D112/D115, [`fold_order`] ([`FOLD_ORDER`]): base → base/region → fired profile
//!   rules (by priority, stable) → industry → industry/region → per department in declared order: its archetype
//!   (D107, deduplicated, at its first position: the base `department_archetype` row, then the D116
//!   `base_dept/<a>` layer and its region sub-layer) → the department → the department's region → job → the job's
//!   `channel` dimension layers, in layer-list order; a region sub-layer (D111, `…/region/<r>`) folds only when
//!   `selection.region` names `<r>`; no tenant (D8, refinements apply after the compile);
//! - sibling departments never override each other (D115): a key two departments set differently is a
//!   `department_conflict` unless a job-layer row of that key decides ([`department_conflicts`]);
//! - knowledge (D107): the `stage_knowledge_kind` and `knowledge_pack` rows (and the archetypes' kinds) fold over
//!   the same steps into [`CompiledKnowledge`] ([`fold_knowledge`]);
//! - the selection must hold the D106/D115 hierarchy ([`Selection::hierarchy_issues`]) unless it is the base-only
//!   preview; otherwise it is refused before any layer is chosen;
//! - a layer applies as a set: its adds and patches first, then its tombstones (an entity tombstone cascades to its
//!   attributes and relations, with a `tombstone_cascade` warning);
//! - every error is reported at once, never first-fail.
//!
//! Head compile and pinned compile share [`compile`]: the caller passes the rows to compile (current rows for a
//! preview, tag members for a pinned release). Rows of layers the selection does not choose are ignored. No IO.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::axis::{Condition, Profile};
use crate::canonical::{canonical_json, sha256_hex};
use crate::catalog::{ObjectDef, OperationDefRow, OperationRow, resolve_operations};
use crate::kinds::catalog::SideEffect;
use crate::kinds::instruction::{self, FoldedInstruction, InstructionFields, InstructionFold};
use crate::kinds::journey;
use crate::kinds::overlay::area_of_context_space;
use crate::kinds::profile::AxisRuleFields;
use crate::kinds::role::{DERIVED_ROLES, ExpectedOp, derive_role_grants};
use crate::kinds::stage_knowledge;
use crate::kinds::{Kind, Op, Refusal, validate_key, validate_row};
use crate::layer::{LayerKey, Scope};

/// The error and warning codes a compile reports (the UI's names).
pub mod code {
    /// Refused before compiling: the selection names a profile that does not exist (route 404).
    pub const PROFILE_NOT_FOUND: &str = "profile_not_found";
    pub const INVALID_SELECTION: &str = "invalid_selection";
    pub const PATCH_TARGET_MISSING: &str = "patch_target_missing";
    pub const PARENT_MISSING: &str = "parent_missing";
    pub const DANGLING_RELATION: &str = "dangling_relation";
    pub const UNKNOWN_JOB: &str = "unknown_job";
    pub const UNKNOWN_CONNECTOR_SLOT: &str = "unknown_connector_slot";
    /// Kept from the configurator: `add` of a key an earlier layer already adds.
    pub const DUPLICATE_ADD: &str = "duplicate_add";
    /// Kept from the configurator: `add` of a key an earlier layer tombstoned (directly or by cascade).
    pub const ADD_TOMBSTONED: &str = "add_tombstoned";
    /// Kept from the configurator: a row whose key or fields fail the kind's validator.
    pub const INVALID_ROW: &str = "invalid_row";
    /// Kept from the configurator: a row of a kind its layer's scope does not allow.
    pub const KIND_NOT_ALLOWED_IN_LAYER: &str = "kind_not_allowed_in_layer";
    /// Warning: an entity tombstone also removed attributes / relations.
    pub const TOMBSTONE_CASCADE: &str = "tombstone_cascade";
    /// Warning: an effective entity has no attribute.
    pub const ENTITY_WITHOUT_ATTRIBUTES: &str = "entity_without_attributes";
    /// Pinned compile (agent-api `compile_pinned`): the selection needs a layer whose family no pin names.
    pub const UNPINNED_LAYER: &str = "unpinned_layer";
    /// D106/D115: refused before compiling, the selection has no industry or no department.
    pub const HIERARCHY_INCOMPLETE: &str = "hierarchy_incomplete";
    /// D115: refused before compiling, a department of another industry than the selection's.
    pub const DEPARTMENT_INDUSTRY_MISMATCH: &str = "department_industry_mismatch";
    /// D115: refused before compiling, a department selected twice.
    pub const DUPLICATE_DEPARTMENT: &str = "duplicate_department";
    /// Warning: the base-only preview compiled (no industry, department, profile or job) — not a job.
    pub const BASE_ONLY_PREVIEW: &str = "base_only_preview";
    /// D115: two sibling departments (or their archetypes) set one key to different values, or one removes what
    /// the other sets, and no job-layer row of the key decides. `key` = `<kind>:<key>`; the message names the
    /// layers ([`super::DepartmentConflict`] carries them as a list).
    pub const DEPARTMENT_CONFLICT: &str = "department_conflict";
    /// D107: a lower layer sets `required: false` on a knowledge kind an earlier layer made required.
    pub const KIND_REQUIRED_DOWNGRADE: &str = "kind_required_downgrade";
    /// D115: two departments pin one pack key at two different versions (a release freezes one version per pack).
    pub const PACK_VERSION_CONFLICT: &str = "pack_version_conflict";
    /// D107, listed (a warning, never an error): a folded pack reference that matches none of the job's effective
    /// knowledge kinds.
    pub const PACK_NOT_APPLICABLE: &str = "pack_not_applicable";
    /// D108: a lower layer loosens (or removes) a `job_setting` rule floor an earlier layer set.
    pub const FLOOR_LOOSENED: &str = "floor_loosened";
    /// D111: refused before compiling (agent-api, against the platform's `region` vocabulary): the selection names a
    /// region that is not a value of the platform `region` dimension.
    pub const UNKNOWN_REGION: &str = "unknown_region";
}

/// The D106/D115 codes of [`Selection::hierarchy_issues`]: a compile error list holding one of them is a 422
/// refusal under that code (see [`refusal`]), never an ordinary `compile_failed`.
pub const HIERARCHY_CODES: [&str; 3] = [
    code::HIERARCHY_INCOMPLETE,
    code::DEPARTMENT_INDUSTRY_MISMATCH,
    code::DUPLICATE_DEPARTMENT,
];

// --- inputs -------------------------------------------------------------------------------------------------------

/// The selection's dimension values (D59): `channel` qualifies job layers. Region is not a dimension any more: it is
/// [`Selection::region`] (D111).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SelectionDimensions {
    #[serde(default)]
    pub channel: Vec<String>,
}

/// D65 as amended by D106/D111/D115: what a job is built from (UI `Selection`, `types/core.ts`). A job selects one
/// `industry` and one or more of its `departments` (`industry_dept/<industry>/<dept>`, primary first, no duplicates);
/// only the base-only compile *preview* ([`Selection::is_base_only_preview`]) may leave both empty. Validation lives
/// in one place, [`Selection::hierarchy_issues`]; every entry point (template registration, job creation, compile,
/// cut) refuses with it. Deserialised directly, a legacy selection (`industry_depts`, `dimensions.region`) reads as
/// one without departments; agent-api's readers upgrade a stored one first (`design::read::selection_of_value`,
/// Ruling R15: a pre-KH release snapshot is immutable), and one still without departments is refused
/// `hierarchy_incomplete` — never silently compiled as base only.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Selection {
    /// Business-model profile key; null = no profile rules.
    #[serde(default)]
    pub profile: Option<String>,
    /// D106: the industry key; empty only in the base-only preview (a stored `null` reads as empty).
    #[serde(default, deserialize_with = "null_as_empty")]
    pub industry: String,
    /// D115: the departments, each an `industry_dept/<industry>/<dept>` layer key of `industry`, in declared order;
    /// `departments[0]` is the primary department. A stored `null` reads as none (refused `hierarchy_incomplete`,
    /// never a shape error's 500).
    #[serde(default, deserialize_with = "null_as_empty")]
    pub departments: Vec<String>,
    /// Area keys inside the chosen departments (D61); empty = every area.
    #[serde(default)]
    pub areas: Vec<String>,
    #[serde(default)]
    pub dimensions: SelectionDimensions,
    /// D111: one optional region (a jurisdiction code): selects the `…/region/<r>` sub-layers of the base, the industry
    /// and each department (absent = none applies). Its vocabulary is the platform's `region` dimension values
    /// (agent-api `platform::templates::region_refusal`, 422 `unknown_region`).
    #[serde(default)]
    pub region: Option<String>,
}

/// A `null` field as its empty value (`""`, `[]`).
fn null_as_empty<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

/// One reason a selection breaks the D106/D115 hierarchy: `code` (`hierarchy_incomplete`,
/// `department_industry_mismatch`, `duplicate_department`, `invalid_selection`), the selection `path`
/// (`selection.industry`, `selection.departments`, `selection.departments[<n>]`) and a sentence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectionIssue {
    pub code: String,
    pub path: String,
    pub message: String,
}

impl SelectionIssue {
    fn new(code: &str, path: impl Into<String>, message: String) -> Self {
        Self {
            code: code.to_owned(),
            path: path.into(),
            message,
        }
    }
}

/// The `(industry, department)` of a department key `industry_dept/<industry>/<dept>`; `None` for any other key.
pub fn parse_department(key: &str) -> Option<(String, String)> {
    let k = LayerKey::parse_label(key).ok()?;
    if k.scope != Scope::IndustryDept {
        return None;
    }
    Some((k.industry?, k.department?))
}

/// The department key `industry_dept/<industry>/<dept>`.
pub fn department_key(industry: &str, department: &str) -> String {
    format!("industry_dept/{industry}/{department}")
}

impl Selection {
    /// The values the selection names for a platform dimension/grouping key (`area`, `channel`, `region`); empty
    /// for any other key (web `selectionValues`).
    pub fn values(&self, key: &str) -> &[String] {
        match key {
            "area" => self.areas.as_slice(),
            "channel" => self.dimensions.channel.as_slice(),
            "region" => self.region.as_slice(),
            _ => &[],
        }
    }

    /// The primary department (`departments[0]`), if any.
    pub fn primary_department(&self) -> Option<&str> {
        self.departments.first().map(String::as_str)
    }

    /// Whether `industry_dept/<industry>/<department>` is one of the selected departments.
    pub fn has_department(&self, industry: &str, department: &str) -> bool {
        self.departments
            .iter()
            .any(|d| *d == department_key(industry, department))
    }

    /// The base-only compile preview (no profile, industry, department or job): the only selection allowed without
    /// the hierarchy, and labelled `base_only_preview` by the compile. Never a job's selection.
    pub fn is_base_only_preview(&self, job: Option<&str>) -> bool {
        self.profile.as_deref().is_none_or(str::is_empty)
            && self.industry.is_empty()
            && self.departments.is_empty()
            && job.is_none_or(str::is_empty)
    }

    /// Every way the selection breaks D106/D115, in path order (empty = the hierarchy holds): no industry or no
    /// department (`hierarchy_incomplete`), an entry that is not a department key (`invalid_selection`), a
    /// department of another industry (`department_industry_mismatch`), a department listed twice
    /// (`duplicate_department`). Whether each department layer exists is the compile's check, not this one.
    pub fn hierarchy_issues(&self) -> Vec<SelectionIssue> {
        let mut out = Vec::new();
        if self.industry.is_empty() {
            out.push(SelectionIssue::new(
                code::HIERARCHY_INCOMPLETE,
                "selection.industry",
                "Choose the industry the job belongs to.".to_owned(),
            ));
        }
        if self.departments.is_empty() {
            out.push(SelectionIssue::new(
                code::HIERARCHY_INCOMPLETE,
                "selection.departments",
                "Choose at least one department of the job's industry.".to_owned(),
            ));
        }
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        for (n, dept) in self.departments.iter().enumerate() {
            let path = format!("selection.departments[{n}]");
            match parse_department(dept) {
                None => out.push(SelectionIssue::new(
                    code::INVALID_SELECTION,
                    path,
                    format!(
                        "“{dept}” is not a department (industry_dept/<industry>/<department>)."
                    ),
                )),
                Some((industry, _)) if !self.industry.is_empty() && industry != self.industry => {
                    out.push(SelectionIssue::new(
                        code::DEPARTMENT_INDUSTRY_MISMATCH,
                        path,
                        format!(
                            "{dept} is a department of {industry}, not of {}.",
                            self.industry
                        ),
                    ))
                }
                Some(_) if !seen.insert(dept.as_str()) => out.push(SelectionIssue::new(
                    code::DUPLICATE_DEPARTMENT,
                    path,
                    format!("{dept} is selected twice."),
                )),
                Some(_) => {}
            }
        }
        out
    }
}

/// UI `CompileTarget`: a selection plus the job layer on top (null = no job layer).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CompileTarget {
    pub selection: Selection,
    #[serde(default)]
    pub job: Option<String>,
}

/// A layer as the compile reads it; serialises as the UI `Layer` (`id`, `scope`, the key parts, `extends`, `tag`,
/// `tagged_at`). `tag`/`tagged_at` are display data the caller fills (null when unknown).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayerMeta {
    pub id: String,
    #[serde(flatten)]
    pub key: LayerKey,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extends: Option<String>,
    #[serde(default)]
    pub tag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tagged_at: Option<String>,
}

impl LayerMeta {
    /// The layer key / web `layerLabel` (`industry_dept/retail/customer_service`).
    pub fn label(&self) -> String {
        self.key.label()
    }
}

/// One layer row revision the compile reads. The caller chooses which: current rows (head) or tag members
/// (pinned). `fields` is the row's JSON object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Row {
    pub id: String,
    pub layer_id: String,
    pub kind: Kind,
    pub key: String,
    pub op: Op,
    pub fields: Value,
    /// The row revision in its layer; 1 for a row a compiler or a fixture synthesises. A `patch` past its first
    /// revision is an in-place edit of the layer's own row ([`Fold::apply_catalogue`] / [`Fold::apply_overlay`]).
    #[serde(default = "first_revision")]
    pub rev: i32,
}

fn first_revision() -> i32 {
    1
}

/// An axis rule: when `condition` holds for the profile, the profile layer `layer` (`profile/<rule>`) applies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AxisRuleDef {
    pub key: String,
    pub priority: i64,
    pub condition: Condition,
    /// The profile layer key (`profile/<rule>`).
    pub layer: String,
}

impl AxisRuleDef {
    /// The rule definitions the `axis_rule` rows carry (pinned compile), in row order; tombstones are left out.
    /// A row whose fields do not parse is an `invalid_row` error (every one reported); the caller returns the
    /// errors as the compile's (`compile_failed`), never a silently smaller rule set.
    pub fn from_rows(rows: &[Row]) -> Result<Vec<AxisRuleDef>, Vec<CompileError>> {
        let mut defs = Vec::new();
        let mut errors = Vec::new();
        for r in rows
            .iter()
            .filter(|r| r.kind == Kind::AxisRule && r.op != Op::Tombstone)
        {
            match serde_json::from_value::<AxisRuleFields>(r.fields.clone()) {
                Ok(f) => defs.push(AxisRuleDef {
                    key: r.key.clone(),
                    priority: f.priority,
                    condition: f.condition,
                    layer: f.layer,
                }),
                Err(e) => errors.push(definition_error(r, &e.to_string())),
            }
        }
        if errors.is_empty() {
            Ok(defs)
        } else {
            Err(errors)
        }
    }
}

/// `invalid_row` for a definition row (`axis_rule`, `profile`) the compile cannot read.
fn definition_error(row: &Row, problem: &str) -> CompileError {
    CompileError::new(
        code::INVALID_ROW,
        &row.key,
        Some(row.id.as_str()),
        format!(
            "{} {} (layer {}): {problem}",
            row.kind, row.key, row.layer_id
        ),
    )
}

/// A business-model profile: its key and axis values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileDef {
    pub key: String,
    pub selection: Profile,
}

impl ProfileDef {
    /// The profiles the `profile` rows define (tombstones left out), in row order. When two rows share a key the
    /// later one wins in [`compile`] (the web builds a `Map` from the rows). A row without a readable `selection`
    /// is an `invalid_row` error (every one reported), as for [`AxisRuleDef::from_rows`].
    pub fn from_rows(rows: &[Row]) -> Result<Vec<ProfileDef>, Vec<CompileError>> {
        let mut defs = Vec::new();
        let mut errors = Vec::new();
        for r in rows
            .iter()
            .filter(|r| r.kind == Kind::Profile && r.op != Op::Tombstone)
        {
            let parsed = match r.fields.get("selection") {
                Some(v) => serde_json::from_value::<Profile>(v.clone()).map_err(|e| e.to_string()),
                None => Err("no selection".to_owned()),
            };
            match parsed {
                Ok(selection) => defs.push(ProfileDef {
                    key: r.key.clone(),
                    selection,
                }),
                Err(problem) => errors.push(definition_error(r, &problem)),
            }
        }
        if errors.is_empty() {
            Ok(defs)
        } else {
            Err(errors)
        }
    }
}

/// An `industry × department` cell that exists (an `industry_dept` layer).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DepartmentRef {
    pub industry: String,
    pub department: String,
}

impl DepartmentRef {
    /// Every `industry_dept` layer of `layers` (not their region sub-layers, D111).
    pub fn from_layers(layers: &[LayerMeta]) -> Vec<DepartmentRef> {
        layers
            .iter()
            .filter(|l| l.key.scope == Scope::IndustryDept && l.key.region.is_none())
            .filter_map(|l| {
                Some(DepartmentRef {
                    industry: l.key.industry.clone()?,
                    department: l.key.department.clone()?,
                })
            })
            .collect()
    }
}

/// Everything a compile reads. Order matters where the web's does: `layers` (first match wins), `rows` (applied in
/// this order within a layer) and `rules` (equal priorities keep this order).
#[derive(Debug, Clone, Copy)]
pub struct CompileInput<'a> {
    pub layers: &'a [LayerMeta],
    pub rows: &'a [Row],
    pub rules: &'a [AxisRuleDef],
    pub profiles: &'a [ProfileDef],
    pub departments: &'a [DepartmentRef],
}

// --- outputs ------------------------------------------------------------------------------------------------------

/// UI `Items`: key → the item's fields.
pub type Items = BTreeMap<String, Map<String, Value>>;

/// Where an effective item came from: one row, in apply order (UI `Origin`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Origin {
    pub row_id: String,
    pub layer: LayerMeta,
    pub op: Op,
    /// The row's fields as that layer wrote them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fields: Option<Map<String, Value>>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct JobContent {
    pub context_spaces: Items,
    pub procedures: Items,
    pub connectors: Items,
    pub knowledge_stores: Items,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Warning {
    pub code: String,
    pub key: String,
    pub message: String,
}

/// UI `EffectiveConfig`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EffectiveConfig {
    /// [`effective_hash`] of this config.
    pub hash: String,
    pub entities: Items,
    pub attributes: Items,
    pub relations: Items,
    /// The job's fields with `key`, or null without a job.
    pub job: Option<Map<String, Value>>,
    pub job_content: JobContent,
    /// `kind:key` → every row that touched it, in apply order (tombstoned items keep theirs, as the web).
    pub provenance: BTreeMap<String, Vec<Origin>>,
    pub warnings: Vec<Warning>,
    /// Layer keys that applied, in order.
    pub layers: Vec<String>,
    /// Axis rules that fired, in apply order.
    pub rules_fired: Vec<String>,
}

/// UI `CompileError`. `layers` (KH T3): the layers an error names as data — a `department_conflict`'s sibling
/// layers, in fold order; absent from the JSON when empty, so every other error serialises as before.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompileError {
    pub code: String,
    pub key: String,
    pub row_id: Option<String>,
    pub message: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub layers: Vec<String>,
}

impl CompileError {
    fn new(code: &str, key: &str, row_id: Option<&str>, message: String) -> Self {
        Self {
            code: code.to_owned(),
            key: key.to_owned(),
            row_id: row_id.map(str::to_owned),
            message,
            layers: Vec::new(),
        }
    }

    /// The error naming `layers` as data ([`CompileError::layers`]).
    pub fn with_layers(mut self, layers: Vec<String>) -> Self {
        self.layers = layers;
        self
    }
}

/// The refusal a failed compile maps to before it is a `compile_failed`: 404 `profile_not_found` (returned
/// alone, before any compile), or 422 under the first D106/D115 code ([`HIERARCHY_CODES`]) when the selection
/// breaks the hierarchy (the route lists every issue as `errors[{path, message}]`, path = the error's `key`).
/// `None` for ordinary compile errors (422 `compile_failed` + `errors[]`).
pub fn refusal(errors: &[CompileError]) -> Option<Refusal> {
    if let [e] = errors
        && e.code == code::PROFILE_NOT_FOUND
    {
        return Some(Refusal::new(
            404,
            code::PROFILE_NOT_FOUND,
            e.message.clone(),
        ));
    }
    if !errors
        .iter()
        .any(|e| HIERARCHY_CODES.contains(&e.code.as_str()))
    {
        return None;
    }
    issues_refusal(errors.iter().map(|e| (e.code.as_str(), e.message.as_str())))
}

/// THE refusal of a list of D106/D115 selection issues, as `(code, message)` (a [`SelectionIssue`] or a
/// [`selection_refusal`] compile error): 422 under the first issue whose code is one of [`HIERARCHY_CODES`], else
/// `invalid_selection`; the message joins every issue's. `None` for no issue. The pinned compile ([`refusal`]) and
/// the API's entry points both map through this one function.
pub fn issues_refusal<'a>(issues: impl IntoIterator<Item = (&'a str, &'a str)>) -> Option<Refusal> {
    let issues: Vec<(&str, &str)> = issues.into_iter().collect();
    if issues.is_empty() {
        return None;
    }
    let code = issues
        .iter()
        .find_map(|(c, _)| HIERARCHY_CODES.into_iter().find(|h| h == c))
        .unwrap_or(code::INVALID_SELECTION);
    let message = issues.iter().map(|(_, m)| *m).collect::<Vec<_>>().join(" ");
    Some(Refusal::new(422, code, message))
}

/// Who compiles: a job context (design load, demo, readiness, cut, `PUT /target`, the pinned compile) never gets the
/// base-only exemption; only the platform compile page's explicit [`Mode::Preview`] does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Every compile of a job: the D106/D115 hierarchy is required.
    Job,
    /// The platform compile preview: the base-only selection (no profile, industry, department or job) compiles,
    /// labelled `base_only_preview`; anything else still needs the hierarchy.
    Preview,
}

/// [`Selection::hierarchy_issues`] as compile errors (`key` = the selection path); nothing only for the base-only
/// selection under [`Mode::Preview`]. [`compile`] and [`selected_layers`] return them alone, before choosing a
/// layer.
pub fn selection_refusal(
    selection: &Selection,
    job: Option<&str>,
    mode: Mode,
) -> Vec<CompileError> {
    if mode == Mode::Preview && selection.is_base_only_preview(job) {
        return Vec::new();
    }
    selection
        .hierarchy_issues()
        .into_iter()
        .map(|i| CompileError {
            code: i.code,
            key: i.path,
            row_id: None,
            message: i.message,
            layers: Vec::new(),
        })
        .collect()
}

/// One folded catalogue item: its key, its fields (the whole item) and the row/layer that last set them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatalogueItem {
    pub key: String,
    pub row_id: String,
    /// The layer key of that row.
    pub layer: String,
    pub fields: Map<String, Value>,
}

/// The catalogue kinds folded over the same layers (add sets, patch replaces the whole item, tombstone removes), in
/// first-add order. `operations` are resolved (plan §4): [`resolve_operations`] over the folded objects and
/// operation rows, each `*` row followed by its variants, with its generated `error_states`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct CompiledCatalogue {
    pub objects: Vec<CatalogueItem>,
    pub operations: Vec<OperationRow>,
    pub error_states: Vec<CatalogueItem>,
    pub gates: Vec<CatalogueItem>,
    /// D119: a qualifier or post-gate role no layer gives grants carries the ones derived from `expectations`
    /// ([`derive_role_grants`], marked `derived`); a role a layer sets keeps its own grants.
    pub agent_roles: Vec<CatalogueItem>,
    /// D119: the job's `operation_expectation` rows folded over the same layers, in first-expected order: a later
    /// layer's expectation of an operation replaces an earlier one's, a tombstone removes it. Absent when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expectations: Vec<ExpectedOp>,
    /// Folded per key (D108): a patch without `value` keeps the nearest value above it; floors only tighten.
    pub job_settings: Vec<CatalogueItem>,
    /// 0222: the folded `rule_slot` rows (key = the rule id), whole items, in first-add order. Absent when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rule_slots: Vec<CatalogueItem>,
    /// D108: the folded `instruction` slots ([`instruction`] module docs), in slot order; absent when empty, so a
    /// compile without instructions serialises as before.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub instructions: Vec<FoldedInstruction>,
}

/// A successful compile.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Compiled {
    pub effective: EffectiveConfig,
    pub catalogue: CompiledCatalogue,
    /// D107/D115: the job's effective knowledge kinds and pack references ([`fold_knowledge`]).
    #[serde(default)]
    pub knowledge: CompiledKnowledge,
    /// Every row the compile applied (overlay and catalogue), in apply order.
    pub provenance_rows: Vec<String>,
}

/// `"sha256:" + hex(SHA-256(canonical JSON))` of the UI `EffectiveConfig` minus `hash`, `provenance` and
/// `warnings` (plan §6).
pub fn effective_hash(cfg: &EffectiveConfig) -> String {
    let v = serde_json::json!({
        "entities": cfg.entities,
        "attributes": cfg.attributes,
        "relations": cfg.relations,
        "job": cfg.job,
        "job_content": cfg.job_content,
        "layers": cfg.layers,
        "rules_fired": cfg.rules_fired,
    });
    format!("sha256:{}", sha256_hex(&canonical_json(&v)))
}

// --- layer order --------------------------------------------------------------------------------------------------

fn nonempty(s: Option<&str>) -> Option<&str> {
    s.filter(|v| !v.is_empty())
}

fn first(layers: &[LayerMeta], pred: impl Fn(&LayerKey) -> bool) -> Option<&LayerMeta> {
    layers.iter().find(|l| pred(&l.key))
}

/// The fired rules, by priority; equal priorities keep the input order (a stable sort, as `Array.sort`).
fn fired_rules<'r>(rules: &'r [AxisRuleDef], profile: Option<&Profile>) -> Vec<&'r AxisRuleDef> {
    let Some(profile) = profile else {
        return Vec::new();
    };
    let mut fired: Vec<&AxisRuleDef> = rules.iter().filter(|r| r.condition.eval(profile)).collect();
    fired.sort_by_key(|r| r.priority);
    fired
}

// --- the one fold order (D112, D115) ------------------------------------------------------------------------------

/// A position of THE fold order (D112 as amended by D115). Every kind of layered content — rows, knowledge kinds,
/// packs, instructions, settings, floors — folds in [`FOLD_ORDER`]; [`fold_order`] lays a selection out over it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FoldPosition {
    Base,
    /// D111: `base/region/<r>` of the selection's region.
    BaseRegion,
    /// The fired axis rules' `profile/<rule>` layers, by priority.
    Profile,
    Industry,
    /// D111: `industry/<i>/region/<r>` of the selection's region.
    IndustryRegion,
    /// D60/D107: the department's archetype (a `department_archetype` row of base, then the D116 `base_dept/<a>`
    /// layer), deduplicated: an archetype two departments extend folds once, at its first position.
    Archetype,
    /// D116 + D111: `base_dept/<a>/region/<r>` of the selection's region, right after its `base_dept/<a>` layer.
    ArchetypeRegion,
    /// An `industry_dept` layer; archetype → department → department region repeat per department (D115).
    Department,
    /// D111: `industry_dept/<i>/<d>/region/<r>` of the selection's region, right after its department (D115 (7)).
    DepartmentRegion,
    Job,
    /// The job's selected `channel` dimension layers.
    Channel,
    /// D8: a tenant's refinements (`job_overlay_row`), applied by agent-api on top of the compile, never by it.
    Tenant,
}

/// The positions of THE fold order, in order (D112, D115).
pub const FOLD_ORDER: [FoldPosition; 12] = [
    FoldPosition::Base,
    FoldPosition::BaseRegion,
    FoldPosition::Profile,
    FoldPosition::Industry,
    FoldPosition::IndustryRegion,
    FoldPosition::Archetype,
    FoldPosition::ArchetypeRegion,
    FoldPosition::Department,
    FoldPosition::DepartmentRegion,
    FoldPosition::Job,
    FoldPosition::Channel,
    FoldPosition::Tenant,
];

/// What a fold step applies: a layer, or a department archetype (by its `department_archetype` key).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FoldSource<'l> {
    Layer(&'l LayerMeta),
    Archetype(&'l str),
}

/// One step of [`fold_order`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FoldStep<'l> {
    pub position: FoldPosition,
    pub source: FoldSource<'l>,
    /// D115: the index in `selection.departments` of the department group the step belongs to (its archetype, the
    /// department, its region sub-layer); `None` outside the department steps.
    pub department: Option<usize>,
}

impl<'l> FoldStep<'l> {
    /// The step's layer (`None` for an archetype).
    pub fn layer(&self) -> Option<&'l LayerMeta> {
        match self.source {
            FoldSource::Layer(l) => Some(l),
            FoldSource::Archetype(_) => None,
        }
    }

    /// The archetype key of an archetype-position step of either source (D116): the legacy
    /// `department_archetype` step's key, or the department of a `base_dept/<a>` layer (or its region sub-layer)
    /// folded at [`FoldPosition::Archetype`] / [`FoldPosition::ArchetypeRegion`]; `None` for every other step.
    pub fn archetype(&self) -> Option<&'l str> {
        match self.source {
            FoldSource::Archetype(a) => Some(a),
            FoldSource::Layer(l) => match self.position {
                FoldPosition::Archetype | FoldPosition::ArchetypeRegion => {
                    l.key.department.as_deref()
                }
                _ => None,
            },
        }
    }

    /// The step's label: the layer key, or `department_archetype/<key>`.
    pub fn label(&self) -> String {
        match self.source {
            FoldSource::Layer(l) => l.label(),
            FoldSource::Archetype(a) => archetype_layer_id(a),
        }
    }

    /// The `layer_id` the step's rows carry: the layer's id, or [`archetype_layer_id`] (the rows
    /// [`archetype_rows`] makes).
    pub fn row_layer_id(&self) -> String {
        match self.source {
            FoldSource::Layer(l) => l.id.clone(),
            FoldSource::Archetype(a) => archetype_layer_id(a),
        }
    }
}

/// The step label and row `layer_id` of archetype `key`: `department_archetype/<key>`.
pub fn archetype_layer_id(key: &str) -> String {
    format!("department_archetype/{key}")
}

fn step(position: FoldPosition, source: FoldSource<'_>, department: Option<usize>) -> FoldStep<'_> {
    FoldStep {
        position,
        source,
        department,
    }
}

/// THE fold order (D112/D115) of `selection` (+ `job`) over `layers` (first match in `layers` wins, as the web
/// `compileModel`): base → base/region → the `fired` rules' profile layers → industry → industry/region → for each
/// selected department in declared order: its archetype (the department layer's `extends` `<a>`; once, at its first
/// position: the `department_archetype` step, then the D116 `base_dept/<a>` layer if it exists, then its region
/// sub-layer) → the department → its region sub-layer → the job layer → its selected `channel` dimension layers. A
/// region sub-layer (D111) is a step only when `selection.region` names its region and the layer exists; without a
/// region no sub-layer applies. Tenant refinements are not a compile step (D8). Selection validation is not this function's: a selection without the
/// hierarchy simply yields fewer steps.
pub fn fold_order<'l>(
    layers: &'l [LayerMeta],
    selection: &Selection,
    job: Option<&str>,
    fired: &[&AxisRuleDef],
) -> Vec<FoldStep<'l>> {
    use FoldPosition as P;
    let region = nonempty(selection.region.as_deref());
    // D111: the region sub-layer of `parent` for the selection's region, if both exist
    let sub = |parent: &LayerMeta| -> Option<&'l LayerMeta> {
        let r = region?;
        first(layers, |k| {
            k.region.as_deref() == Some(r) && k.parent() == parent.key
        })
    };
    let mut steps: Vec<FoldStep<'l>> = Vec::new();
    if let Some(l) = first(layers, |k| k.scope == Scope::Base && k.region.is_none()) {
        steps.push(step(P::Base, FoldSource::Layer(l), None));
        if let Some(r) = sub(l) {
            steps.push(step(P::BaseRegion, FoldSource::Layer(r), None));
        }
    }
    for rule in fired {
        if let Some(l) = layers.iter().find(|l| l.label() == rule.layer) {
            steps.push(step(P::Profile, FoldSource::Layer(l), None));
        }
    }
    if let Some(industry) = nonempty(Some(selection.industry.as_str())) {
        if let Some(l) = first(layers, |k| {
            k.scope == Scope::Industry
                && k.industry.as_deref() == Some(industry)
                && k.region.is_none()
        }) {
            steps.push(step(P::Industry, FoldSource::Layer(l), None));
            if let Some(r) = sub(l) {
                steps.push(step(P::IndustryRegion, FoldSource::Layer(r), None));
            }
        }
        let mut archetypes: BTreeSet<&'l str> = BTreeSet::new();
        for (n, dept) in selection.departments.iter().enumerate() {
            let Some(l) = first(layers, |k| {
                k.scope == Scope::IndustryDept
                    && k.industry.as_deref() == Some(industry)
                    && k.label() == *dept
            }) else {
                continue;
            };
            if let Some(a) = nonempty(l.extends.as_deref())
                && archetypes.insert(a)
            {
                steps.push(step(P::Archetype, FoldSource::Archetype(a), Some(n)));
                // D116: the industry-independent department layer of the archetype, then its region sub-layer
                if let Some(b) = first(layers, |k| {
                    k.scope == Scope::BaseDept
                        && k.department.as_deref() == Some(a)
                        && k.region.is_none()
                }) {
                    steps.push(step(P::Archetype, FoldSource::Layer(b), Some(n)));
                    if let Some(r) = sub(b) {
                        steps.push(step(P::ArchetypeRegion, FoldSource::Layer(r), Some(n)));
                    }
                }
            }
            steps.push(step(P::Department, FoldSource::Layer(l), Some(n)));
            if let Some(r) = sub(l) {
                steps.push(step(P::DepartmentRegion, FoldSource::Layer(r), Some(n)));
            }
        }
    }
    if let Some(job) = nonempty(job) {
        if let Some(l) = first(layers, |k| {
            k.scope == Scope::Job && k.job.as_deref() == Some(job) && k.dimension.is_none()
        }) {
            steps.push(step(P::Job, FoldSource::Layer(l), None));
        }
        for l in layers {
            let k = &l.key;
            // D111: a region is a sub-layer of base / industry / department, never a job dimension.
            if k.scope == Scope::Job
                && k.job.as_deref() == Some(job)
                && let Some((dim, value)) = &k.dimension
                && dim != "region"
                && selection.values(dim).contains(value)
            {
                steps.push(step(P::Channel, FoldSource::Layer(l), None));
            }
        }
    }
    steps
}

/// The layers of [`fold_order`], in order (archetype steps carry no layer).
fn choose_layers<'l>(
    layers: &'l [LayerMeta],
    sel: &Selection,
    job: Option<&str>,
    fired: &[&AxisRuleDef],
) -> Vec<&'l LayerMeta> {
    fold_order(layers, sel, job, fired)
        .iter()
        .filter_map(FoldStep::layer)
        .collect()
}

/// A selected department no `industry_dept` layer defines (the hierarchy itself is [`selection_refusal`]'s).
fn selection_errors(sel: &Selection, departments: &[DepartmentRef]) -> Vec<CompileError> {
    let mut errors = Vec::new();
    for (n, dept) in sel.departments.iter().enumerate() {
        let Some((industry, name)) = parse_department(dept) else {
            continue;
        };
        if !departments
            .iter()
            .any(|d| d.industry == industry && d.department == name)
        {
            errors.push(CompileError::new(
                code::INVALID_SELECTION,
                &format!("selection.departments[{n}]"),
                None,
                format!("{industry} has no department “{name}”."),
            ));
        }
    }
    errors
}

// --- fold ---------------------------------------------------------------------------------------------------------

const ENTITY: usize = 0;
const ATTRIBUTE: usize = 1;
const RELATION: usize = 2;
const JOB: usize = 3;
const CONTEXT_SPACE: usize = 4;
const PROCEDURE: usize = 5;
const CONNECTOR: usize = 6;
const KNOWLEDGE_STORE: usize = 7;

fn overlay_slot(kind: Kind) -> Option<usize> {
    match kind {
        Kind::Entity => Some(ENTITY),
        Kind::Attribute => Some(ATTRIBUTE),
        Kind::Relation => Some(RELATION),
        Kind::Job => Some(JOB),
        Kind::ContextSpace => Some(CONTEXT_SPACE),
        Kind::Procedure => Some(PROCEDURE),
        Kind::ConnectorSlot => Some(CONNECTOR),
        Kind::KnowledgeStore => Some(KNOWLEDGE_STORE),
        _ => None,
    }
}

fn catalogue_slot(kind: Kind) -> Option<usize> {
    match kind {
        Kind::Object => Some(0),
        Kind::Operation => Some(1),
        Kind::ErrorState => Some(2),
        Kind::GateQuestion => Some(3),
        Kind::AgentRole => Some(4),
        Kind::JobSetting => Some(JOB_SETTING_SLOT),
        Kind::RuleSlot => Some(RULE_SLOT_SLOT),
        _ => None,
    }
}

fn is_catalogue(kind: Kind) -> bool {
    catalogue_slot(kind).is_some()
}

/// D119: the `operation_expectation` rows of `layers` (fold order) folded per operation: a later layer's expectation
/// replaces an earlier one's (keeping its place), a tombstone removes it. The rows the D115 sibling rule keeps out of
/// the fold (`skip`) and the rows a layer may not hold do not count.
fn fold_expectations(
    rows: &[Row],
    layers: &[&LayerMeta],
    skip: &BTreeSet<String>,
) -> Vec<ExpectedOp> {
    let mut out: Vec<ExpectedOp> = Vec::new();
    for layer in layers {
        for row in layer_rows(rows, layer, |k| k == Kind::OperationExpectation) {
            if skip.contains(&row.id) || row_problem(row, layer).is_some() {
                continue;
            }
            let at = out.iter().position(|e| e.op == row.key);
            if row.op == Op::Tombstone {
                if let Some(i) = at {
                    out.remove(i);
                }
                continue;
            }
            let summary = row.fields.get("summary").and_then(Value::as_str);
            let e = ExpectedOp::from_summary(&row.key, summary.unwrap_or_default());
            match at {
                Some(i) => out[i] = e,
                None => out.push(e),
            }
        }
    }
    out
}

/// D119: give a qualifier or post-gate role with no grants the ones its job's expectations derive.
fn derive_grants(
    item: &mut CatalogueItem,
    expectations: &[ExpectedOp],
    operations: &[OperationRow],
) {
    if !DERIVED_ROLES.contains(&item.key.as_str()) {
        return;
    }
    let set = item
        .fields
        .get("grants")
        .and_then(Value::as_array)
        .is_some_and(|g| !g.is_empty());
    if set {
        return;
    }
    let max = item
        .fields
        .get("max_side_effect")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or(SideEffect::R);
    let grants = derive_role_grants(&item.key, max, expectations, operations);
    if let Ok(v) = serde_json::to_value(grants) {
        item.fields.insert("grants".to_owned(), v);
    }
}

/// The activity question: its options are the job's own activities, one per `stage:activity` (the keys the GA
/// route hint reads, `job_design::gates::route_hint`).
pub const ACTIVITY_GATE: &str = "GA";

/// Replace the options of the activity gate (`GA`) by the job's own activities, `generated: true`, in the order of
/// `activities` (key order). The library row declares no options of its own, so no job classifies intent against
/// another industry's labels (an IT desk never offers "Track a parcel"). An archetype's activity (stage `*`) has no
/// stage column and is not an option, as it is no overlay base ([`FoldedActivity`]). Each option describes itself with
/// the activity's plain `description`, or its label when the row has none. A compile without the gate in its catalogue is left
/// alone.
fn generate_activity_options(gates: &mut [CatalogueItem], activities: &[FoldedActivity]) {
    let Some(gate) = gates.iter_mut().find(|g| g.key == ACTIVITY_GATE) else {
        return;
    };
    let options: Vec<Value> = activities
        .iter()
        .filter(|a| a.stage != stage_knowledge::ANY_STAGE)
        .map(|a| {
            let description = if a.description.is_empty() {
                &a.label
            } else {
                &a.description
            };
            serde_json::json!({
                "key": a.key,
                "label": a.label,
                "description": description,
                "generated": true,
            })
        })
        .collect();
    gate.fields
        .insert("options".to_owned(), Value::Array(options));
}

/// The provenance key `kind:key` (web `pk`).
fn pk(kind: Kind, key: &str) -> String {
    format!("{}:{key}", kind.as_str())
}

/// A relation key names `entity` as its subject or object (`Order|paidBy|Payment`).
fn touches(relation: &str, entity: &str) -> bool {
    relation
        .split('|')
        .enumerate()
        .any(|(i, part)| i != 1 && part == entity)
}

struct Entry {
    fields: Map<String, Value>,
    row_id: String,
    layer: String,
}

/// Items in insertion order, with the JS object semantics the web relies on: re-setting a key keeps its place,
/// removing and re-adding moves it to the end.
#[derive(Default)]
struct Bucket {
    order: Vec<String>,
    items: BTreeMap<String, Entry>,
}

impl Bucket {
    fn contains(&self, key: &str) -> bool {
        self.items.contains_key(key)
    }

    fn get(&self, key: &str) -> Option<&Entry> {
        self.items.get(key)
    }

    fn get_mut(&mut self, key: &str) -> Option<&mut Entry> {
        self.items.get_mut(key)
    }

    fn keys(&self) -> &[String] {
        &self.order
    }

    fn set(&mut self, key: &str, entry: Entry) {
        if self.items.insert(key.to_owned(), entry).is_none() {
            self.order.push(key.to_owned());
        }
    }

    fn remove(&mut self, key: &str) {
        if self.items.remove(key).is_some() {
            self.order.retain(|k| k != key);
        }
    }

    fn to_items(&self) -> Items {
        self.items
            .iter()
            .map(|(k, e)| (k.clone(), e.fields.clone()))
            .collect()
    }

    /// Items in insertion order.
    fn ordered(&self) -> impl Iterator<Item = (&String, &Entry)> {
        self.order
            .iter()
            .filter_map(|k| self.items.get(k).map(|e| (k, e)))
    }

    fn into_ordered(self) -> Vec<CatalogueItem> {
        let Bucket { order, mut items } = self;
        order
            .into_iter()
            .filter_map(|key| {
                let e = items.remove(&key)?;
                Some(CatalogueItem {
                    key,
                    row_id: e.row_id,
                    layer: e.layer,
                    fields: e.fields,
                })
            })
            .collect()
    }
}

/// A layer's rows of the wanted kinds as one set: adds and patches first, then tombstones, each in row order.
fn layer_rows<'r>(rows: &'r [Row], layer: &LayerMeta, wanted: fn(Kind) -> bool) -> Vec<&'r Row> {
    let mut ordered: Vec<&Row> = Vec::new();
    let mut tombstones: Vec<&Row> = Vec::new();
    for row in rows
        .iter()
        .filter(|r| r.layer_id == layer.id && wanted(r.kind))
    {
        if row.op == Op::Tombstone {
            tombstones.push(row);
        } else {
            ordered.push(row);
        }
    }
    ordered.extend(tombstones);
    ordered
}

/// `kind_not_allowed_in_layer` or `invalid_row` for a row the compile must not apply.
fn row_problem(row: &Row, layer: &LayerMeta) -> Option<CompileError> {
    if layer.key.is_region() && !row.kind.allowed_in_region() {
        return Some(CompileError::new(
            code::KIND_NOT_ALLOWED_IN_LAYER,
            &row.key,
            Some(row.id.as_str()),
            format!(
                "{} rows are not allowed in region sub-layers ({}).",
                row.kind,
                layer.label()
            ),
        ));
    }
    if !row.kind.allowed_in(layer.key.scope) {
        return Some(CompileError::new(
            code::KIND_NOT_ALLOWED_IN_LAYER,
            &row.key,
            Some(row.id.as_str()),
            format!(
                "{} rows are not allowed in {} layers ({}).",
                row.kind,
                layer.key.scope,
                layer.label()
            ),
        ));
    }
    let mut problems: Vec<String> = Vec::new();
    if let Err(e) = validate_key(row.kind, &row.key) {
        problems.push(e.to_string());
    }
    if let Err(es) = validate_row(row.kind, row.op, &row.fields) {
        problems.extend(es.iter().map(ToString::to_string));
    }
    if problems.is_empty() {
        return None;
    }
    Some(CompileError::new(
        code::INVALID_ROW,
        &row.key,
        Some(row.id.as_str()),
        format!("{} {}: {}", row.kind, row.key, problems.join("; ")),
    ))
}

/// The `job` row fields that are settings (D108): they fold with the `job_setting` rows of the same key.
const JOB_ROW_SETTINGS: [&str; 2] = ["actor", "stance"];
/// The setting (and `job` row field) that is the job's `identity` instruction (D108).
const PREAMBLE: &str = "preamble";
/// [`catalogue_slot`] of `job_setting`.
const JOB_SETTING_SLOT: usize = 5;
/// [`catalogue_slot`] of `rule_slot`.
const RULE_SLOT_SLOT: usize = 6;

/// The trimmed, non-blank string `field` of `job`'s `job` row in `layer` (the last such row) and that row, if any.
fn job_row_field<'r>(
    rows: &'r [Row],
    layer: &LayerMeta,
    job: &str,
    field: &str,
) -> Option<(&'r Row, String)> {
    rows.iter()
        .rev()
        .filter(|r| {
            r.layer_id == layer.id && r.kind == Kind::Job && r.key == job && r.op != Op::Tombstone
        })
        .find_map(|r| {
            let v = r.fields.get(field).and_then(Value::as_str).map(str::trim)?;
            (!v.is_empty()).then(|| (r, v.to_owned()))
        })
}

/// The layer's `job_setting` row keyed `preamble` (the last one): `Some(None)` for a tombstone, `Some(Some(text))`
/// with its non-blank string `value`, else its non-blank `summary`; `None` without such a row (or one with no text).
/// A row the D115 sibling rule keeps out of the fold (`skip`) does not count.
fn preamble_setting(
    rows: &[Row],
    layer: &LayerMeta,
    skip: &BTreeSet<String>,
) -> Option<Option<String>> {
    let row = rows.iter().rev().find(|r| {
        r.layer_id == layer.id
            && r.kind == Kind::JobSetting
            && r.key == PREAMBLE
            && !skip.contains(&r.id)
    })?;
    if row.op == Op::Tombstone {
        return Some(None);
    }
    let text = |f: &str| {
        row.fields
            .get(f)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_owned)
    };
    text("value").or_else(|| text("summary")).map(Some)
}

/// `floor_loosened` (D108) for `row` of `layer`.
fn floor_loosened(row: &Row, label: &str, problem: &str) -> CompileError {
    CompileError::new(
        code::FLOOR_LOOSENED,
        &row.key,
        Some(row.id.as_str()),
        format!(
            "{label} loosens the rule floor {} {}: {problem}.",
            row.kind, row.key
        ),
    )
}

#[derive(Default)]
struct Fold {
    overlay: [Bucket; 8],
    catalogue: [Bucket; 7],
    /// D115: the rows the sibling rule keeps out of the fold (`Siblings::skip`); every per-row fold skips them.
    skip: BTreeSet<String>,
    /// D115: the keys a job row decides (`Siblings::decided`).
    decided: BTreeSet<(Kind, String)>,
    instructions: InstructionFold,
    /// `(kind, key)` an applied tombstone removed (directly or by cascade).
    removed: BTreeSet<(Kind, String)>,
    provenance: BTreeMap<String, Vec<Origin>>,
    last_row: BTreeMap<String, String>,
    provenance_rows: Vec<String>,
    errors: Vec<CompileError>,
    warnings: Vec<Warning>,
}

impl Fold {
    fn warn(&mut self, code: &str, key: &str, message: String) {
        self.warnings.push(Warning {
            code: code.to_owned(),
            key: key.to_owned(),
            message,
        });
    }

    /// The add-time errors both folds share: `duplicate_add`, `add_tombstoned`.
    fn add_error(&self, bucket: &Bucket, row: &Row, label: &str) -> Option<CompileError> {
        if bucket.contains(&row.key) {
            return Some(CompileError::new(
                code::DUPLICATE_ADD,
                &row.key,
                Some(row.id.as_str()),
                format!(
                    "{label} adds {} {}, which an earlier layer already adds.",
                    row.kind, row.key
                ),
            ));
        }
        if self.removed.contains(&(row.kind, row.key.clone())) {
            return Some(CompileError::new(
                code::ADD_TOMBSTONED,
                &row.key,
                Some(row.id.as_str()),
                format!(
                    "{label} adds {} {}, which an earlier layer tombstoned.",
                    row.kind, row.key
                ),
            ));
        }
        None
    }

    fn patch_missing(row: &Row, label: &str) -> CompileError {
        CompileError::new(
            code::PATCH_TARGET_MISSING,
            &row.key,
            Some(row.id.as_str()),
            format!(
                "{label} patches {} {}, which no earlier layer adds.",
                row.kind, row.key
            ),
        )
    }

    fn apply_overlay(&mut self, layer: &LayerMeta, row: &Row) {
        let Some(slot) = overlay_slot(row.kind) else {
            return;
        };
        if self.skip.contains(&row.id) {
            return;
        }
        if let Some(e) = row_problem(row, layer) {
            self.errors.push(e);
            return;
        }
        let fields = row.fields.as_object().cloned().unwrap_or_default();
        let id = pk(row.kind, &row.key);
        self.provenance.entry(id.clone()).or_default().push(Origin {
            row_id: row.id.clone(),
            layer: layer.clone(),
            op: row.op,
            fields: Some(fields.clone()),
        });
        self.last_row.insert(id, row.id.clone());
        self.provenance_rows.push(row.id.clone());
        let label = layer.label();
        let decided = self.decided.contains(&(row.kind, row.key.clone()));
        match row.op {
            Op::Add => match self.add_error(&self.overlay[slot], row, &label) {
                Some(e) => self.errors.push(e),
                None => self.overlay[slot].set(
                    &row.key,
                    Entry {
                        fields,
                        row_id: row.id.clone(),
                        layer: label,
                    },
                ),
            },
            Op::Patch => match self.overlay[slot].get_mut(&row.key) {
                Some(entry) => {
                    entry.fields.extend(fields);
                    entry.row_id = row.id.clone();
                    entry.layer = label;
                }
                // D115: the job decides a sibling conflict; its patch over the departments' skipped adds is the add
                None if decided => self.overlay[slot].set(
                    &row.key,
                    Entry {
                        fields,
                        row_id: row.id.clone(),
                        layer: label,
                    },
                ),
                // The authoring API stores an edit of a layer's own row as a patch at the next revision; with no
                // earlier layer adding the key, that patch is the row's definition.
                None if row.rev > 1 => self.overlay[slot].set(
                    &row.key,
                    Entry {
                        fields,
                        row_id: row.id.clone(),
                        layer: label,
                    },
                ),
                None => self.errors.push(Self::patch_missing(row, &label)),
            },
            Op::Tombstone => {
                self.overlay[slot].remove(&row.key);
                self.removed.insert((row.kind, row.key.clone()));
                if row.kind == Kind::Entity {
                    self.cascade(&row.key);
                }
            }
        }
    }

    /// An entity tombstone takes its attributes and the relations that name it with it.
    fn cascade(&mut self, entity: &str) {
        let prefix = format!("{entity}.");
        let mut gone: Vec<String> = self.overlay[ATTRIBUTE]
            .keys()
            .iter()
            .filter(|k| k.starts_with(&prefix))
            .cloned()
            .collect();
        gone.extend(
            self.overlay[RELATION]
                .keys()
                .iter()
                .filter(|k| touches(k, entity))
                .cloned(),
        );
        for key in &gone {
            if key.contains('|') {
                self.overlay[RELATION].remove(key);
                self.removed.insert((Kind::Relation, key.clone()));
            } else {
                self.overlay[ATTRIBUTE].remove(key);
                self.removed.insert((Kind::Attribute, key.clone()));
            }
        }
        if !gone.is_empty() {
            self.warn(
                code::TOMBSTONE_CASCADE,
                entity,
                format!("Removing {entity} also removed {}.", gone.join(", ")),
            );
        }
    }

    /// Catalogue rows hold the whole item: add sets, patch replaces, tombstone removes.
    fn apply_catalogue(&mut self, layer: &LayerMeta, row: &Row) {
        let Some(slot) = catalogue_slot(row.kind) else {
            return;
        };
        if self.skip.contains(&row.id) {
            return;
        }
        if let Some(e) = row_problem(row, layer) {
            self.errors.push(e);
            return;
        }
        self.provenance_rows.push(row.id.clone());
        let label = layer.label();
        let fields = row.fields.as_object().cloned().unwrap_or_default();
        let decided = self.decided.contains(&(row.kind, row.key.clone()));
        // D108: a setting inherits — a lower layer's `add` of a setting an earlier layer (or job row) set is its patch
        let op = if row.kind == Kind::JobSetting
            && row.op == Op::Add
            && self.catalogue[slot].contains(&row.key)
        {
            Op::Patch
        } else {
            row.op
        };
        match op {
            Op::Add => match self.add_error(&self.catalogue[slot], row, &label) {
                Some(e) => self.errors.push(e),
                None => self.catalogue[slot].set(
                    &row.key,
                    Entry {
                        fields,
                        row_id: row.id.clone(),
                        layer: label,
                    },
                ),
            },
            Op::Patch => match self.catalogue[slot].get_mut(&row.key) {
                Some(entry) => {
                    let fields = if row.kind == Kind::JobSetting {
                        match journey::fold_setting(&entry.fields, &fields) {
                            Ok(f) => f,
                            Err(problem) => {
                                self.errors.push(floor_loosened(row, &label, &problem));
                                return;
                            }
                        }
                    } else {
                        fields
                    };
                    entry.fields = fields;
                    entry.row_id = row.id.clone();
                    entry.layer = label;
                }
                // D115: as in the overlay fold, the job's patch of a decided key with nothing under it is the add
                None if decided => self.catalogue[slot].set(
                    &row.key,
                    Entry {
                        fields,
                        row_id: row.id.clone(),
                        layer: label,
                    },
                ),
                // As in the overlay fold: an edit of the layer's own row is a patch past its first revision, and
                // with nothing earlier to patch it is the row's definition.
                None if row.rev > 1 => self.catalogue[slot].set(
                    &row.key,
                    Entry {
                        fields,
                        row_id: row.id.clone(),
                        layer: label,
                    },
                ),
                None => self.errors.push(Self::patch_missing(row, &label)),
            },
            Op::Tombstone => {
                if row.kind == Kind::JobSetting
                    && let Some(entry) = self.catalogue[slot].get(&row.key)
                    && let Err(problem) = journey::tombstone_setting(&entry.fields)
                {
                    self.errors.push(floor_loosened(row, &label, &problem));
                    return;
                }
                self.catalogue[slot].remove(&row.key);
                self.removed.insert((row.kind, row.key.clone()));
            }
        }
    }

    /// D108: the `actor` / `stance` a layer's `job` row sets for `job` fold as that layer's `job_setting` of the key
    /// (unless the layer has its own `job_setting` row of it), so every setting takes the nearest value in fold order
    /// whichever row kind set it.
    fn apply_job_row_settings(&mut self, layer: &LayerMeta, rows: &[Row], job: Option<&str>) {
        let Some(job) = job else {
            return;
        };
        let slot = JOB_SETTING_SLOT;
        let label = layer.label();
        for key in JOB_ROW_SETTINGS {
            let own = rows
                .iter()
                .any(|r| r.layer_id == layer.id && r.kind == Kind::JobSetting && r.key == key);
            let Some((row, value)) = job_row_field(rows, layer, job, key) else {
                continue;
            };
            if own {
                continue;
            }
            let mut next = Map::new();
            next.insert("value".to_owned(), Value::String(value));
            match self.catalogue[slot].get_mut(key) {
                Some(entry) => {
                    if let Some(l) = entry.fields.get("label").cloned() {
                        next.insert("label".to_owned(), l);
                    }
                    match journey::fold_setting(&entry.fields, &next) {
                        Ok(f) => {
                            entry.fields = f;
                            entry.row_id = row.id.clone();
                            entry.layer = label.clone();
                        }
                        Err(problem) => self.errors.push(floor_loosened(row, &label, &problem)),
                    }
                }
                None => {
                    next.insert("label".to_owned(), Value::String(key.to_owned()));
                    self.catalogue[slot].set(
                        key,
                        Entry {
                            fields: next,
                            row_id: row.id.clone(),
                            layer: label.clone(),
                        },
                    );
                }
            }
        }
    }

    /// One layer's `instruction` rows (D108). The layer's `identity` comes from, first match wins: its own `identity`
    /// row; its `job_setting` row keyed `preamble` (`value`, else `summary`; a tombstone removes the identity); its
    /// `job` row's `preamble` for `job`. Then the instruction rows: adds and patches, then tombstones.
    fn apply_instructions(&mut self, layer: &LayerMeta, rows: &[Row], job: Option<&str>) {
        let label = layer.label();
        let tag = layer.tag.as_deref();
        // D115: a department's row the sibling rule skips (a job row decides its slot, or an earlier sibling set the
        // same text) never folds — the conflict itself is reported by `sibling_resolution`
        let own: Vec<&Row> = layer_rows(rows, layer, |k| k == Kind::Instruction)
            .into_iter()
            .filter(|r| !self.skip.contains(&r.id))
            .collect();
        let own_identity = own
            .iter()
            .any(|r| r.key == instruction::IDENTITY && r.op != Op::Tombstone);
        if !own_identity {
            match preamble_setting(rows, layer, &self.skip) {
                Some(None) => self.instructions.remove_at(&label, instruction::IDENTITY),
                Some(Some(text)) => self.set_identity(&label, tag, text),
                None => {
                    if let Some((_, text)) =
                        job.and_then(|j| job_row_field(rows, layer, j, PREAMBLE))
                    {
                        self.set_identity(&label, tag, text);
                    }
                }
            }
        }
        for row in own {
            if let Some(e) = row_problem(row, layer) {
                self.errors.push(e);
                continue;
            }
            self.provenance_rows.push(row.id.clone());
            if row.op == Op::Tombstone {
                self.instructions.remove_at(&label, &row.key);
                continue;
            }
            match InstructionFields::deserialize(&row.fields) {
                Ok(f) => self.instructions.set(&label, tag, &row.key, &f),
                Err(e) => self.errors.push(CompileError::new(
                    code::INVALID_ROW,
                    &row.key,
                    Some(row.id.as_str()),
                    format!("instruction {}: {e}", row.key),
                )),
            }
        }
    }

    /// Archetype `key`'s instruction rows ([`archetype_rows`]) at its step: section layer
    /// `department_archetype/<key>`, tagged with the base layer's pin (the archetype row is a base row). A row the
    /// sibling rule skips never folds; provenance names the archetype row itself.
    fn apply_archetype_instructions(&mut self, key: &str, tag: Option<&str>, synthetic: &[Row]) {
        let label = archetype_layer_id(key);
        for row in synthetic
            .iter()
            .filter(|r| r.layer_id == label && r.kind == Kind::Instruction)
            .filter(|r| !self.skip.contains(&r.id))
        {
            let def = row
                .id
                .rsplit_once(ARCHETYPE_INSTRUCTION)
                .map_or(row.id.as_str(), |(def, _)| def);
            if !self.provenance_rows.iter().any(|p| p == def) {
                self.provenance_rows.push(def.to_owned());
            }
            if !instruction::is_slot(&row.key) {
                self.errors.push(CompileError::new(
                    code::INVALID_ROW,
                    &row.key,
                    Some(def),
                    format!(
                        "department archetype {key}: {} is not a prompt slot",
                        row.key
                    ),
                ));
                continue;
            }
            match InstructionFields::deserialize(&row.fields) {
                Ok(f) => self.instructions.set(&label, tag, &row.key, &f),
                Err(e) => self.errors.push(CompileError::new(
                    code::INVALID_ROW,
                    &row.key,
                    Some(def),
                    format!("department archetype {key} instruction {}: {e}", row.key),
                )),
            }
        }
    }

    fn set_identity(&mut self, label: &str, tag: Option<&str>, text: String) {
        let f = InstructionFields {
            text,
            language: None,
            mode: instruction::Mode::Replace,
            summary: None,
        };
        self.instructions.set(label, tag, instruction::IDENTITY, &f);
    }

    fn last(&self, kind: Kind, key: &str) -> Option<String> {
        self.last_row.get(&pk(kind, key)).cloned()
    }

    /// Whole-result checks (web `compileModel` "Validation: report everything").
    fn check(&mut self, job: Option<&str>) {
        let mut found: Vec<CompileError> = Vec::new();
        let entities = &self.overlay[ENTITY];
        for key in self.overlay[ATTRIBUTE].keys() {
            let parent = key.split('.').next().unwrap_or_default();
            if !entities.contains(parent) {
                found.push(CompileError::new(
                    code::PARENT_MISSING,
                    key,
                    self.last(Kind::Attribute, key).as_deref(),
                    format!(
                        "Attribute {key} belongs to {parent}, which this selection does not have."
                    ),
                ));
            }
        }
        for key in self.overlay[RELATION].keys() {
            let parts: Vec<&str> = key.split('|').collect();
            for end in [parts.first(), parts.get(2)].into_iter().flatten() {
                if !end.is_empty() && !entities.contains(end) {
                    found.push(CompileError::new(
                        code::DANGLING_RELATION,
                        key,
                        self.last(Kind::Relation, key).as_deref(),
                        format!(
                            "Relation {key} points at {end}, which this selection does not have."
                        ),
                    ));
                }
            }
        }
        if let Some(job) = job {
            if !self.overlay[JOB].contains(job) {
                found.push(CompileError::new(
                    code::UNKNOWN_JOB,
                    job,
                    None,
                    format!("No base layer defines the job {job}."),
                ));
            }
            let procedures = &self.overlay[PROCEDURE];
            for key in procedures.keys() {
                let Some(entry) = procedures.get(key) else {
                    continue;
                };
                let slots = entry.fields.get("uses_slots").and_then(Value::as_array);
                for slot in slots.into_iter().flatten().filter_map(Value::as_str) {
                    if !self.overlay[CONNECTOR].contains(slot) {
                        found.push(CompileError::new(
                            code::UNKNOWN_CONNECTOR_SLOT,
                            key,
                            self.last(Kind::Procedure, key).as_deref(),
                            format!(
                                "Procedure {key} uses connector slot “{slot}”, which the job does not declare."
                            ),
                        ));
                    }
                }
            }
        }
        self.errors.extend(found);
    }
}

/// The resolved operations of the folded catalogue (plan §4 "operations (resolved + error_states)"). A folded
/// object or operation that does not parse as its catalogue type is an `invalid_row` error.
fn resolve_catalogue(
    objects: &Bucket,
    operations: &Bucket,
) -> Result<Vec<OperationRow>, Vec<CompileError>> {
    let mut errors: Vec<CompileError> = Vec::new();
    let unreadable = |key: &str, e: &Entry, problem: String| {
        CompileError::new(
            code::INVALID_ROW,
            key,
            Some(e.row_id.as_str()),
            format!("{problem} (layer {})", e.layer),
        )
    };
    let mut defs: Vec<ObjectDef> = Vec::new();
    for (key, e) in objects.ordered() {
        match ObjectDef::from_row(key, &Value::Object(e.fields.clone())) {
            Ok(d) => defs.push(d),
            Err(problem) => errors.push(unreadable(key.as_str(), e, problem)),
        }
    }
    let mut rows: Vec<OperationDefRow> = Vec::new();
    for (key, e) in operations.ordered() {
        match OperationDefRow::from_row(key, &Value::Object(e.fields.clone())) {
            Ok(r) => rows.push(r),
            Err(problem) => errors.push(unreadable(key.as_str(), e, problem)),
        }
    }
    if errors.is_empty() {
        Ok(resolve_operations(&rows, &defs))
    } else {
        Err(errors)
    }
}

// --- sibling departments (D115) -----------------------------------------------------------------------------------

/// D115 (3): one key that two or more sibling departments (or their archetypes) set to different values, or that one
/// removes while another sets it. Generic over every row kind: the key is `(kind, key)` of any `layer_row`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DepartmentConflict {
    pub kind: Kind,
    pub key: String,
    /// The department-group steps whose values meet, in fold order (a department layer key, a D116
    /// `base_dept/<a>` layer key (or its region sub-layer), or `department_archetype/<key>`).
    pub layers: Vec<String>,
    /// A job-layer (or job × channel) row of the same key decides: then it is no compile error, and none of the
    /// departments' rows of the key applies (the job's row does).
    pub resolved: bool,
}

/// What the sibling rule decides for a fold.
#[derive(Debug, Clone, Default)]
struct Siblings {
    conflicts: Vec<DepartmentConflict>,
    /// Rows the fold does not apply: a later department's row equal to an earlier one's (a no-op), a later
    /// department's row of an unresolved conflict (the conflict is the error, not a `duplicate_add` after it), and
    /// every department row of a key a job row decides.
    skip: BTreeSet<String>,
    /// `(kind, key)` a job row decides: a job `patch` of such a key with nothing left under it acts as the add.
    decided: BTreeSet<(Kind, String)>,
}

/// Two rows set the same value: both remove it, or both set equal fields.
fn same_value(a: &Row, b: &Row) -> bool {
    let (a_gone, b_gone) = (a.op == Op::Tombstone, b.op == Op::Tombstone);
    a_gone == b_gone && (a_gone || a.fields == b.fields)
}

/// One department group's last row of a key, with every row id the group has for it.
struct GroupValue<'r> {
    department: usize,
    label: String,
    row: &'r Row,
    ids: Vec<String>,
}

/// D115 (3) over `rows` (input rows plus [`archetype_rows`]) laid out by `steps`.
fn sibling_resolution(steps: &[FoldStep<'_>], rows: &[&Row]) -> Siblings {
    let mut by_key: BTreeMap<(Kind, String), Vec<GroupValue<'_>>> = BTreeMap::new();
    let mut job_keys: BTreeSet<(Kind, String)> = BTreeSet::new();
    for s in steps {
        let id = s.row_layer_id();
        for &r in rows.iter().filter(|r| r.layer_id == id) {
            match (s.department, s.position) {
                (Some(d), _) => {
                    let values = by_key.entry((r.kind, r.key.clone())).or_default();
                    match values.last_mut() {
                        // within one department group the later step (department over archetype) wins
                        Some(v) if v.department == d => {
                            v.label = s.label();
                            v.row = r;
                            v.ids.push(r.id.clone());
                        }
                        _ => values.push(GroupValue {
                            department: d,
                            label: s.label(),
                            row: r,
                            ids: vec![r.id.clone()],
                        }),
                    }
                }
                (None, FoldPosition::Job | FoldPosition::Channel) => {
                    job_keys.insert((r.kind, r.key.clone()));
                }
                _ => {}
            }
        }
    }
    let mut out = Siblings::default();
    for ((kind, key), values) in by_key {
        let Some((head, rest)) = values.split_first() else {
            continue;
        };
        if rest.is_empty() {
            continue;
        }
        let later_ids = rest.iter().flat_map(|v| v.ids.iter().cloned());
        if rest.iter().all(|v| same_value(head.row, v.row)) {
            out.skip.extend(later_ids);
            continue;
        }
        let resolved = job_keys.contains(&(kind, key.clone()));
        if resolved {
            out.decided.insert((kind, key.clone()));
            out.skip
                .extend(values.iter().flat_map(|v| v.ids.iter().cloned()));
        } else {
            out.skip.extend(later_ids);
        }
        out.conflicts.push(DepartmentConflict {
            kind,
            key,
            layers: values.iter().map(|v| v.label.clone()).collect(),
            resolved,
        });
    }
    out
}

/// D115 (3): every key sibling departments disagree on under `steps` ([`fold_order`]) over `rows` — resolved ones
/// included (`resolved: true`). Generic over every row kind, so a kind added later (instruction slots, settings) is
/// covered without code here.
pub fn department_conflicts(steps: &[FoldStep<'_>], rows: &[Row]) -> Vec<DepartmentConflict> {
    let synthetic = archetype_rows(rows, steps);
    let all: Vec<&Row> = rows.iter().chain(synthetic.iter()).collect();
    sibling_resolution(steps, &all).conflicts
}

/// The `department_conflict` compile error of an unresolved conflict.
fn conflict_error(c: &DepartmentConflict) -> CompileError {
    CompileError::new(
        code::DEPARTMENT_CONFLICT,
        &format!("{}:{}", c.kind, c.key),
        None,
        format!(
            "The job's departments disagree on {} {}: {} set it differently. A department never overrides a \
             sibling; decide with a row of that key in the job layer — an add (the departments' rows do not apply \
             then), or a patch over what the layers above the departments define (a patch with nothing above it \
             acts as the add).",
            c.kind,
            c.key,
            c.layers.join(", ")
        ),
    )
    .with_layers(c.layers.clone())
}

// --- knowledge (D107) ---------------------------------------------------------------------------------------------

/// D107: a department archetype's knowledge kinds as `stage_knowledge_kind` rows of its fold step (`layer_id` =
/// [`archetype_layer_id`]): for each archetype step of `steps`, the archetype's row in the base step (the last one of
/// its key; a tombstone retires it) gives one `*|<kind>` add, fields `{label: <kind>}`, per plain kind of its
/// `knowledge_kinds`, in entry order. Its `instructions` (owner ruling 2026-10-23) give one `instruction` add per
/// slot, fields = that slot's instruction fields, so the D115 sibling rule covers them like a department's rows. A pattern entry (`policy.*`) declares a family for `pack_kind_undeclared`
/// ([`crate::kinds::stage_knowledge::archetype_covers`]), not a kind.
pub fn archetype_rows(rows: &[Row], steps: &[FoldStep<'_>]) -> Vec<Row> {
    let base: BTreeSet<String> = steps
        .iter()
        .filter(|s| s.position == FoldPosition::Base)
        .map(FoldStep::row_layer_id)
        .collect();
    let mut out = Vec::new();
    for s in steps {
        let FoldSource::Archetype(key) = s.source else {
            continue;
        };
        let def = rows.iter().rev().find(|r| {
            r.kind == Kind::DepartmentArchetype && r.key == key && base.contains(&r.layer_id)
        });
        let Some(def) = def.filter(|d| d.op != Op::Tombstone) else {
            continue;
        };
        for (name, text) in archetype_activities(&def.fields) {
            let mut fields = Map::new();
            fields.insert("label".to_owned(), Value::String(text.to_owned()));
            out.push(Row {
                id: format!("{}@{name}", def.id),
                layer_id: archetype_layer_id(key),
                kind: Kind::Activity,
                key: format!("{}:{name}", stage_knowledge::ANY_STAGE),
                op: Op::Add,
                fields: Value::Object(fields),
                rev: 1,
            });
        }
        for kind in archetype_kinds(&def.fields) {
            let mut fields = Map::new();
            fields.insert("label".to_owned(), Value::String(kind.to_owned()));
            out.push(Row {
                id: format!("{}#{kind}", def.id),
                layer_id: archetype_layer_id(key),
                kind: Kind::StageKnowledgeKind,
                key: format!("{}|{kind}", stage_knowledge::ANY_STAGE),
                op: Op::Add,
                fields: Value::Object(fields),
                rev: 1,
            });
        }
        for (slot, fields) in archetype_instructions(&def.fields) {
            out.push(Row {
                id: format!("{}{}{slot}", def.id, ARCHETYPE_INSTRUCTION),
                layer_id: archetype_layer_id(key),
                kind: Kind::Instruction,
                key: slot.to_owned(),
                op: Op::Add,
                fields: fields.clone(),
                rev: 1,
            });
        }
    }
    out
}

/// The separator of an archetype instruction row's id: `<archetype row id>!<slot>`.
const ARCHETYPE_INSTRUCTION: char = '!';

/// An archetype row's `instructions` (owner ruling 2026-10-23) as `(slot, instruction fields)`, in slot-key order;
/// the row was validated on write, so a slot or value the instruction fold cannot read is reported there
/// ([`Fold::apply_archetype_instructions`]), never dropped silently here.
fn archetype_instructions(fields: &Value) -> Vec<(&str, &Value)> {
    fields
        .get("instructions")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .map(|(slot, f)| (slot.as_str(), f))
        .collect()
}

/// An archetype row's `activities` (free text) as `(name, text)`: the name is the text in snake case (`explain a
/// bill` → `explain_a_bill`); an entry with no letter or digit is skipped.
fn archetype_activities(fields: &Value) -> Vec<(String, &str)> {
    fields
        .get("activities")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter_map(|text| {
            let mut name = String::new();
            for c in text.trim().chars().flat_map(char::to_lowercase) {
                if c.is_ascii_alphanumeric() {
                    name.push(c);
                } else if !name.ends_with('_') && !name.is_empty() {
                    name.push('_');
                }
            }
            let name = name.trim_end_matches('_').to_owned();
            (!name.is_empty()).then_some((name, text.trim()))
        })
        .collect()
}

/// The plain kinds of an archetype row's `knowledge_kinds` (entries a `*|<kind>` key accepts), in entry order.
fn archetype_kinds(fields: &Value) -> Vec<&str> {
    fields
        .get("knowledge_kinds")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|k| {
            crate::kinds::split_stage_knowledge_key(&format!("{}|{k}", stage_knowledge::ANY_STAGE))
                .is_some()
        })
        .collect()
}

/// One effective knowledge kind of a job (D107): the fold of its `stage_knowledge_kind` rows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FoldedKind {
    /// A stage, or `*` (every stage).
    pub stage: String,
    pub kind: String,
    pub label: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store_key: Option<String>,
    /// Only ever false → true down the fold.
    pub required: bool,
    /// D110: frozen packs alone do not satisfy it.
    pub tenant_data: bool,
    /// The step that declared it: a layer key, or `department_archetype/<key>`.
    pub layer: String,
    /// The step that set it last.
    pub set_by: String,
    /// D115: the departments (`industry_dept/<i>/<d>`) whose steps declared or set it, in declared order; empty =
    /// inherited by every department (base, industry, job). An archetype's kind belongs to every selected
    /// department that extends the archetype.
    pub departments: Vec<String>,
    /// The row that set it last.
    pub row_id: String,
}

impl FoldedKind {
    /// The key `stage|kind`.
    pub fn key(&self) -> String {
        format!("{}|{}", self.stage, self.kind)
    }

    /// Whether department `dept` (`industry_dept/<i>/<d>`) has this kind: its own, or inherited.
    pub fn of_department(&self, dept: &str) -> bool {
        self.departments.is_empty() || self.departments.iter().any(|d| d == dept)
    }
}

/// One folded `knowledge_pack` reference (PK-1): pack `pack_key@version` for `stage` / `kind` (either may be `*`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FoldedPack {
    pub stage: String,
    pub kind: String,
    pub pack_key: String,
    pub version: i64,
    /// D112: the pack key an upgrade renamed this pack from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replaces: Option<String>,
    /// The layer of the row that set it.
    pub layer: String,
    pub row_id: String,
    /// D115: the department whose step set it (empty = not a department layer).
    pub departments: Vec<String>,
}

impl FoldedPack {
    /// The row key `stage|kind|pack_key`.
    pub fn key(&self) -> String {
        format!("{}|{}|{}", self.stage, self.kind, self.pack_key)
    }

    /// `pack_key@version`.
    pub fn pin(&self) -> String {
        format!("{}@{}", self.pack_key, self.version)
    }

    /// Whether the reference reaches kind `k` (`*` on either side matches any stage; a `*` pack kind any kind).
    pub fn applies_to(&self, k: &FoldedKind) -> bool {
        let any = crate::kinds::knowledge_pack::ANY;
        (self.stage == any || k.stage == stage_knowledge::ANY_STAGE || self.stage == k.stage)
            && (self.kind == any || self.kind == k.kind)
    }
}

/// One effective activity of a job (D107: an archetype's activities fold before its department, as stage `*`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FoldedActivity {
    /// `stage:name`; an archetype's activity is `*:<name>` (name = its text in snake case).
    pub key: String,
    /// A stage, or `*` (every stage).
    pub stage: String,
    pub name: String,
    pub label: String,
    /// The winning row's plain-language `description` (empty when it has none): the description of the activity's
    /// generated GA option.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// The winning row's technical `summary` (empty when it has none), carried so a designer save writes it back.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub summary: String,
    /// The step that declared it: a layer key, or `department_archetype/<key>`.
    pub layer: String,
    pub set_by: String,
    /// As [`FoldedKind::departments`].
    pub departments: Vec<String>,
    pub row_id: String,
    /// B8 T2b: the objects the activity reads or writes (the winning row's `objects`; empty when it names none).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub objects: Vec<journey::ActivityObjectRef>,
    /// The operations it expects (the winning row's `operations`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub operations: Vec<journey::OperationRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<journey::ActivityActor>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub trigger: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub also_in_stages: Vec<String>,
    /// D61: the department area.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub area: Option<String>,
    /// Owner 2026-10-26: the winning row marks the activity required (a tenant cannot hide it).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub required: bool,
}

impl FoldedActivity {
    /// Whether department `dept` has this activity: its own, or inherited.
    pub fn of_department(&self, dept: &str) -> bool {
        self.departments.is_empty() || self.departments.iter().any(|d| d == dept)
    }
}

/// D107/D115: a job's effective knowledge — the union over its departments plus the inherited kinds — and its
/// effective activities (the archetypes' included).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CompiledKnowledge {
    /// By key (`stage|kind`, bytewise).
    pub kinds: Vec<FoldedKind>,
    /// By key (`stage:name`, bytewise).
    #[serde(default)]
    pub activities: Vec<FoldedActivity>,
    /// Every folded pack reference, by row key (bytewise).
    pub packs: Vec<FoldedPack>,
    /// D107: the references of `packs` that match none of `kinds` — listed, never an error (a base pack for IT-desk
    /// kinds does not break a retail customer-care job).
    pub not_applicable: Vec<FoldedPack>,
}

impl CompiledKnowledge {
    /// D115 (4): the kinds department `dept` (`industry_dept/<i>/<d>`) searches — its own plus the inherited ones —
    /// each with its source layer ([`FoldedKind::layer`]).
    pub fn kinds_of_department(&self, dept: &str) -> Vec<&FoldedKind> {
        self.kinds
            .iter()
            .filter(|k| k.of_department(dept))
            .collect()
    }
}

/// The departments a step's rows belong to: the department of a department step; for an archetype-position step of
/// either source ([`FoldStep::archetype`], D116) every selected department that extends it.
fn step_departments(
    s: &FoldStep<'_>,
    steps: &[FoldStep<'_>],
    selection: &Selection,
) -> Vec<String> {
    match s.archetype() {
        Some(a) => steps
            .iter()
            .filter(|d| d.position == FoldPosition::Department)
            .filter(|d| d.layer().and_then(|l| nonempty(l.extends.as_deref())) == Some(a))
            .filter_map(|d| {
                d.department
                    .and_then(|n| selection.departments.get(n))
                    .cloned()
            })
            .collect(),
        None => s
            .department
            .and_then(|n| selection.departments.get(n))
            .cloned()
            .into_iter()
            .collect(),
    }
}

/// Add the departments of `from` to `to` that it lacks, in order.
fn union_into(to: &mut Vec<String>, from: &[String]) {
    for d in from {
        if !to.contains(d) {
            to.push(d.clone());
        }
    }
}

/// The knowledge fold over laid-out steps (see [`fold_knowledge`]).
fn knowledge_of(
    steps: &[FoldStep<'_>],
    rows: &[&Row],
    skip: &BTreeSet<String>,
    selection: &Selection,
) -> (CompiledKnowledge, Vec<CompileError>) {
    let mut errors: Vec<CompileError> = Vec::new();
    // key → (kind, declared by an archetype and touched by nothing else)
    let mut kinds: BTreeMap<String, (FoldedKind, bool)> = BTreeMap::new();
    let mut packs: BTreeMap<String, FoldedPack> = BTreeMap::new();
    // key → (activity, declared by an archetype and touched by nothing else)
    let mut activities: BTreeMap<String, (FoldedActivity, bool)> = BTreeMap::new();
    let wanted = |k: Kind| {
        matches!(
            k,
            Kind::StageKnowledgeKind | Kind::KnowledgePack | Kind::Activity
        )
    };
    for s in steps {
        let id = s.row_layer_id();
        let label = s.label();
        // D116: a base_dept layer at the archetype position gives way as the archetype does
        let archetype = s.archetype().is_some();
        let depts = step_departments(s, steps, selection);
        let mut ordered: Vec<&Row> = Vec::new();
        let mut tombstones: Vec<&Row> = Vec::new();
        for &r in rows
            .iter()
            .filter(|r| r.layer_id == id && wanted(r.kind) && !skip.contains(&r.id))
        {
            if r.op == Op::Tombstone {
                tombstones.push(r);
            } else {
                ordered.push(r);
            }
        }
        ordered.extend(tombstones);
        for r in ordered {
            if let Some(layer) = s.layer()
                && let Some(e) = row_problem(r, layer)
            {
                errors.push(e);
                continue;
            }
            match r.kind {
                Kind::StageKnowledgeKind => {
                    fold_kind(&mut kinds, &mut errors, r, &label, archetype, &depts);
                }
                Kind::KnowledgePack => fold_pack(&mut packs, r, &label, &depts),
                Kind::Activity => fold_activity(&mut activities, r, &label, archetype, &depts),
                _ => {}
            }
        }
    }
    // an archetype's `*|kind` gives way, per department, to the same kind placed at a stage for that department
    let placed: Vec<(String, Vec<String>)> = kinds
        .values()
        .filter(|(k, _)| k.stage != stage_knowledge::ANY_STAGE)
        .map(|(k, _)| (k.kind.clone(), k.departments.clone()))
        .collect();
    let kinds: Vec<FoldedKind> = kinds
        .into_values()
        .filter_map(|(mut k, archetype_only)| {
            if !archetype_only || k.stage != stage_knowledge::ANY_STAGE {
                return Some(k);
            }
            let same = placed
                .iter()
                .filter(|(kind, _)| *kind == k.kind)
                .map(|(_, d)| d.as_slice());
            give_way(&mut k.departments, same).then_some(k)
        })
        .collect();
    // an archetype's activity gives way, per department, to the same activity (name or label) a department declares
    let declared: Vec<(String, String, Vec<String>)> = activities
        .values()
        .filter(|(_, archetype_only)| !archetype_only)
        .map(|(a, _)| {
            (
                a.name.clone(),
                a.label.to_lowercase(),
                a.departments.clone(),
            )
        })
        .collect();
    let activities: Vec<FoldedActivity> = activities
        .into_values()
        .filter_map(|(mut a, archetype_only)| {
            if !archetype_only {
                return Some(a);
            }
            let label = a.label.to_lowercase();
            let same = declared
                .iter()
                .filter(|(name, l, _)| *name == a.name || *l == label)
                .map(|(_, _, d)| d.as_slice());
            give_way(&mut a.departments, same).then_some(a)
        })
        .collect();
    let packs: Vec<FoldedPack> = packs.into_values().collect();
    errors.extend(pack_version_conflicts(&packs));
    let not_applicable: Vec<FoldedPack> = packs
        .iter()
        .filter(|p| !kinds.iter().any(|k| p.applies_to(k)))
        .cloned()
        .collect();
    (
        CompiledKnowledge {
            kinds,
            activities,
            packs,
            not_applicable,
        },
        errors,
    )
}

/// One `stage_knowledge_kind` row onto the folded kinds: an add or patch sets (a later layer patches label,
/// description, store key, `tenant_data`; `required` only false → true), a tombstone removes.
fn fold_kind(
    kinds: &mut BTreeMap<String, (FoldedKind, bool)>,
    errors: &mut Vec<CompileError>,
    r: &Row,
    label: &str,
    archetype: bool,
    depts: &[String],
) {
    if r.op == Op::Tombstone {
        kinds.remove(&r.key);
        return;
    }
    let Some((stage, kind)) = crate::kinds::split_stage_knowledge_key(&r.key) else {
        return;
    };
    let f = match stage_knowledge::StageKnowledgeKindFields::deserialize(&r.fields) {
        Ok(f) => f,
        Err(e) => {
            errors.push(CompileError::new(
                code::INVALID_ROW,
                &r.key,
                Some(r.id.as_str()),
                format!("stage_knowledge_kind {}: {e}", r.key),
            ));
            return;
        }
    };
    let required = f.required_flag();
    match kinds.get_mut(&r.key) {
        Some((k, archetype_only)) => {
            if k.required && required == Some(false) {
                errors.push(CompileError::new(
                    code::KIND_REQUIRED_DOWNGRADE,
                    &r.key,
                    Some(r.id.as_str()),
                    format!(
                        "{label} makes the knowledge kind {} optional, but {} made it required; a lower layer may \
                         only make a kind required.",
                        r.key, k.set_by
                    ),
                ));
                return;
            }
            k.label = f.label.trim().to_owned();
            k.description = f.description_text();
            if let Some(store) = f.store() {
                k.store_key = Some(store);
            }
            k.required = k.required || required == Some(true);
            if let Some(t) = f.tenant_data {
                k.tenant_data = t;
            }
            label.clone_into(&mut k.set_by);
            k.row_id.clone_from(&r.id);
            union_into(&mut k.departments, depts);
            *archetype_only = *archetype_only && archetype;
        }
        None => {
            kinds.insert(
                r.key.clone(),
                (
                    FoldedKind {
                        stage: stage.to_owned(),
                        kind: kind.to_owned(),
                        label: f.label.trim().to_owned(),
                        description: f.description_text(),
                        store_key: f.store(),
                        required: required == Some(true),
                        tenant_data: f.tenant_data == Some(true),
                        layer: label.to_owned(),
                        set_by: label.to_owned(),
                        departments: depts.to_vec(),
                        row_id: r.id.clone(),
                    },
                    archetype,
                ),
            );
        }
    }
}

/// D107: an archetype item (kind or activity) of `departments` gives way to the same item declared for a department
/// (`same` = the departments of each such declaration; empty = every department): those departments drop out.
/// `true` while some department still has the archetype's item.
fn give_way<'d>(departments: &mut Vec<String>, same: impl Iterator<Item = &'d [String]>) -> bool {
    for d in same {
        if d.is_empty() {
            departments.clear();
        } else {
            departments.retain(|x| !d.contains(x));
        }
    }
    !departments.is_empty()
}

/// One `activity` row onto the folded activities: an add or patch sets (the whole item), a tombstone removes.
fn fold_activity(
    activities: &mut BTreeMap<String, (FoldedActivity, bool)>,
    r: &Row,
    label: &str,
    archetype: bool,
    depts: &[String],
) {
    if r.op == Op::Tombstone {
        activities.remove(&r.key);
        return;
    }
    let (stage, name) = r
        .key
        .split_once(':')
        .unwrap_or((stage_knowledge::ANY_STAGE, r.key.as_str()));
    let text = r
        .fields
        .get("label")
        .and_then(Value::as_str)
        .unwrap_or(name)
        .trim()
        .to_owned();
    // B8 T2b: the row's typed fields (a row that does not parse is `row_problem`'s to report; it carries none here)
    let typed: Option<journey::ActivityFields> = serde_json::from_value(r.fields.clone()).ok();
    let (objects, operations, actor, trigger, also, area, required, description, summary) =
        match typed {
            Some(f) => (
                f.objects.unwrap_or_default(),
                f.operations.unwrap_or_default(),
                f.actor,
                f.trigger.unwrap_or_default(),
                f.also_in_stages.unwrap_or_default(),
                f.area,
                f.required,
                f.description.unwrap_or_default().trim().to_owned(),
                f.summary.unwrap_or_default().trim().to_owned(),
            ),
            None => Default::default(),
        };
    match activities.get_mut(&r.key) {
        Some((a, archetype_only)) => {
            // an add or patch sets the whole item
            a.objects = objects;
            a.operations = operations;
            a.actor = actor;
            a.trigger = trigger;
            a.also_in_stages = also;
            a.area = area;
            a.required = required;
            a.description = description;
            a.summary = summary;
            a.label = text;
            label.clone_into(&mut a.set_by);
            a.row_id.clone_from(&r.id);
            union_into(&mut a.departments, depts);
            *archetype_only = *archetype_only && archetype;
        }
        None => {
            activities.insert(
                r.key.clone(),
                (
                    FoldedActivity {
                        key: r.key.clone(),
                        stage: stage.to_owned(),
                        name: name.to_owned(),
                        label: text,
                        description,
                        summary,
                        layer: label.to_owned(),
                        set_by: label.to_owned(),
                        departments: depts.to_vec(),
                        row_id: r.id.clone(),
                        objects,
                        operations,
                        actor,
                        trigger,
                        also_in_stages: also,
                        area,
                        required,
                    },
                    archetype,
                ),
            );
        }
    }
}

/// One `knowledge_pack` row onto the folded references: the same key in a later step replaces the earlier one, a
/// tombstone removes it.
fn fold_pack(packs: &mut BTreeMap<String, FoldedPack>, r: &Row, label: &str, depts: &[String]) {
    if r.op == Op::Tombstone {
        packs.remove(&r.key);
        return;
    }
    let Some((stage, kind, pack_key)) = crate::kinds::knowledge_pack::split_key(&r.key) else {
        return;
    };
    let Some(version) = r.fields.get("version").and_then(Value::as_i64) else {
        return;
    };
    let replaces = r
        .fields
        .get("replaces")
        .and_then(Value::as_str)
        .map(str::to_owned);
    packs.insert(
        r.key.clone(),
        FoldedPack {
            stage: stage.to_owned(),
            kind: kind.to_owned(),
            pack_key: pack_key.to_owned(),
            version,
            replaces,
            layer: label.to_owned(),
            row_id: r.id.clone(),
            departments: depts.to_vec(),
        },
    );
}

/// D115 (4): one pack key at two versions from two different departments (a release freezes one version of a
/// pack). Within one department, or from inherited layers, PK-1's per-kind precedence still decides.
fn pack_version_conflicts(packs: &[FoldedPack]) -> Vec<CompileError> {
    let mut errors = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for (i, a) in packs.iter().enumerate() {
        if a.departments.is_empty() || seen.contains(&a.pack_key) {
            continue;
        }
        let clash: Vec<&FoldedPack> = packs[i + 1..]
            .iter()
            .filter(|b| {
                b.pack_key == a.pack_key
                    && b.version != a.version
                    && !b.departments.is_empty()
                    && b.departments != a.departments
            })
            .collect();
        if clash.is_empty() {
            continue;
        }
        seen.insert(a.pack_key.clone());
        let pins: Vec<String> = std::iter::once(a)
            .chain(clash)
            .map(|p| format!("{} ({})", p.pin(), p.layer))
            .collect();
        errors.push(CompileError::new(
            code::PACK_VERSION_CONFLICT,
            &a.pack_key,
            Some(a.row_id.as_str()),
            format!(
                "The job's departments pin the pack {} at different versions: {}. A release freezes one version \
                 of a pack: add a knowledge_pack tombstone of one department's reference key (stage|kind|pack) to \
                 the job layer, so one version remains.",
                a.pack_key,
                pins.join(", ")
            ),
        ));
    }
    errors
}

/// D107/D112/D115: the knowledge fold alone over already-loaded `layers` and `rows` — [`fold_order`] without profile
/// layers (they hold neither kinds nor packs, PK-1), the archetypes' kinds ([`archetype_rows`]), the sibling rule
/// ([`department_conflicts`], unresolved ones are errors), then the kinds and the pack references. agent-api's
/// template seeding reads it, so a job's materialised kinds are the compile's. The errors (`department_conflict`,
/// `kind_required_downgrade`, `pack_version_conflict`, `invalid_row`, `kind_not_allowed_in_layer`) never stop the
/// fold: the compile fails on them; a caller that only seeds may log them. No selection validation here.
pub fn fold_knowledge(
    layers: &[LayerMeta],
    rows: &[Row],
    selection: &Selection,
    job: Option<&str>,
) -> (CompiledKnowledge, Vec<CompileError>) {
    let steps = fold_order(layers, selection, job, &[]);
    let synthetic = archetype_rows(rows, &steps);
    let all: Vec<&Row> = rows.iter().chain(synthetic.iter()).collect();
    let siblings = sibling_resolution(&steps, &all);
    let mut errors: Vec<CompileError> = siblings
        .conflicts
        .iter()
        .filter(|c| !c.resolved)
        .map(conflict_error)
        .collect();
    let (knowledge, more) = knowledge_of(&steps, &all, &siblings.skip, selection);
    errors.extend(more);
    (knowledge, errors)
}

// --- compile ------------------------------------------------------------------------------------------------------

/// The selection's profile (the last definition of its key wins); `Err([profile_not_found])` for an unknown one.
fn resolve_profile<'p>(
    profiles: &'p [ProfileDef],
    selection: &Selection,
) -> Result<Option<&'p Profile>, Vec<CompileError>> {
    let Some(key) = nonempty(selection.profile.as_deref()) else {
        return Ok(None);
    };
    match profiles.iter().rev().find(|p| p.key == key) {
        Some(p) => Ok(Some(&p.selection)),
        None => Err(vec![CompileError::new(
            code::PROFILE_NOT_FOUND,
            key,
            None,
            format!("No profile {key}."),
        )]),
    }
}

/// The layer keys [`compile`] would apply for `selection` (+ `job`) over `input`, in apply order, without folding a
/// row (`input.rows` is not read). Refused exactly as [`compile`] refuses an unknown profile. Pinned compile uses it
/// to find the layers whose family no pin names (`unpinned_layer`).
pub fn selected_layers(
    input: &CompileInput<'_>,
    selection: &Selection,
    job: Option<&str>,
    mode: Mode,
) -> Result<Vec<String>, Vec<CompileError>> {
    let refused = selection_refusal(selection, nonempty(job), mode);
    if !refused.is_empty() {
        return Err(refused);
    }
    let profile = resolve_profile(input.profiles, selection)?;
    let fired = fired_rules(input.rules, profile);
    Ok(
        choose_layers(input.layers, selection, nonempty(job), &fired)
            .iter()
            .map(|l| l.label())
            .collect(),
    )
}

/// Compile `selection` (+ the `job` layer) over `input`. Head compile passes the current rows, pinned compile the
/// members of the pinned tags; both run this one function.
///
/// A selection that breaks the D106/D115 hierarchy is refused with only its [`selection_refusal`] errors — always:
/// only [`compile_preview`] lets the base-only selection through (with a `base_only_preview` warning). A selection naming an unknown
/// profile is refused alone with `profile_not_found` (see [`refusal`]: the route's 404). Otherwise every problem
/// comes back at once: `invalid_selection`, `kind_not_allowed_in_layer`,
/// `invalid_row`, `duplicate_add`, `add_tombstoned`, `patch_target_missing`, `parent_missing`,
/// `dangling_relation`, `unknown_job`, `unknown_connector_slot`, `floor_loosened` (D108), `department_conflict` (D115),
/// `kind_required_downgrade` (D107), `pack_version_conflict` (D115). `pack_not_applicable` (D107) is a warning and
/// listed in [`CompiledKnowledge::not_applicable`].
///
/// Instructions (D108) fold after the catalogue, layer by layer in the same [`fold_order`] ([`instruction`] module
/// docs); a job's `preamble` is its `identity` instruction. The D115 sibling rule covers them like every other row
/// kind: a department's instruction row a job row decides (or a later sibling's equal one) is in the fold's sibling skip set.
pub fn compile(
    input: &CompileInput<'_>,
    selection: &Selection,
    job: Option<&str>,
) -> Result<Compiled, Vec<CompileError>> {
    compile_as(input, selection, job, Mode::Job)
}

/// [`compile`] for the platform compile page ([`Mode::Preview`]): the base-only selection compiles, labelled.
pub fn compile_preview(
    input: &CompileInput<'_>,
    selection: &Selection,
    job: Option<&str>,
) -> Result<Compiled, Vec<CompileError>> {
    compile_as(input, selection, job, Mode::Preview)
}

/// [`compile`] / [`compile_preview`] by `mode`.
pub fn compile_as(
    input: &CompileInput<'_>,
    selection: &Selection,
    job: Option<&str>,
    mode: Mode,
) -> Result<Compiled, Vec<CompileError>> {
    let job = nonempty(job);
    let refused = selection_refusal(selection, job, mode);
    if !refused.is_empty() {
        return Err(refused);
    }
    let profile = resolve_profile(input.profiles, selection)?;

    let fired = fired_rules(input.rules, profile);
    let steps = fold_order(input.layers, selection, job, &fired);
    let chosen: Vec<&LayerMeta> = steps.iter().filter_map(FoldStep::layer).collect();
    // D107: the archetypes' kinds are rows of their steps; D115: siblings never override each other
    let synthetic = archetype_rows(input.rows, &steps);
    let all_rows: Vec<&Row> = input.rows.iter().chain(synthetic.iter()).collect();
    let siblings = sibling_resolution(&steps, &all_rows);

    let mut fold = Fold {
        errors: selection_errors(selection, input.departments),
        skip: siblings.skip,
        decided: siblings.decided,
        ..Fold::default()
    };
    fold.errors.extend(
        siblings
            .conflicts
            .iter()
            .filter(|c| !c.resolved)
            .map(conflict_error),
    );
    for layer in &chosen {
        for row in layer_rows(input.rows, layer, Kind::is_overlay) {
            fold.apply_overlay(layer, row);
        }
    }
    fold.check(job);
    for layer in &chosen {
        for row in layer_rows(input.rows, layer, is_catalogue) {
            fold.apply_catalogue(layer, row);
        }
        fold.apply_job_row_settings(layer, input.rows, job);
    }
    // D111: the platform-wide definitions (`axis_rule`, `profile`, `department_archetype`) never apply from a region
    // sub-layer — reported, never folded (the schema refuses them since 0139)
    for layer in chosen.iter().filter(|l| l.key.is_region()) {
        fold.errors.extend(
            input
                .rows
                .iter()
                .filter(|r| r.layer_id == layer.id && !r.kind.allowed_in_region())
                .filter_map(|r| row_problem(r, layer)),
        );
    }
    // the archetype's instructions fold at its step, between the industry and its department (D112)
    let base_tag = steps
        .iter()
        .find(|s| s.position == FoldPosition::Base)
        .and_then(FoldStep::layer)
        .and_then(|l| l.tag.as_deref());
    for s in &steps {
        match s.source {
            FoldSource::Layer(layer) => fold.apply_instructions(layer, input.rows, job),
            FoldSource::Archetype(key) => {
                fold.apply_archetype_instructions(key, base_tag, &synthetic)
            }
        }
    }
    let resolved = resolve_catalogue(&fold.catalogue[0], &fold.catalogue[1]);
    let operations = match resolved {
        Ok(ops) => ops,
        Err(errors) => {
            fold.errors.extend(errors);
            Vec::new()
        }
    };
    let (knowledge, knowledge_errors) = knowledge_of(&steps, &all_rows, &fold.skip, selection);
    fold.errors.extend(knowledge_errors);
    if !fold.errors.is_empty() {
        return Err(fold.errors);
    }
    let expectations = fold_expectations(input.rows, &chosen, &fold.skip);

    let Fold {
        overlay,
        catalogue,
        instructions,
        provenance,
        provenance_rows,
        mut warnings,
        ..
    } = fold;

    let areas = &selection.areas;
    let context_spaces: Items = overlay[CONTEXT_SPACE]
        .to_items()
        .into_iter()
        .filter(|(k, _)| {
            let area = area_of_context_space(k).unwrap_or("");
            areas.is_empty() || areas.iter().any(|a| a.as_str() == area)
        })
        .collect();
    let attribute_keys = overlay[ATTRIBUTE].keys();
    for key in overlay[ENTITY].keys() {
        let prefix = format!("{key}.");
        if attribute_keys.iter().any(|a| a.starts_with(&prefix)) {
            continue;
        }
        let label = overlay[ENTITY]
            .get(key)
            .and_then(|e| e.fields.get("label"))
            .and_then(Value::as_str)
            .unwrap_or(key.as_str());
        warnings.push(Warning {
            code: code::ENTITY_WITHOUT_ATTRIBUTES.to_owned(),
            key: key.clone(),
            message: format!("{label} has no attributes yet."),
        });
    }
    for p in &knowledge.not_applicable {
        warnings.push(Warning {
            code: code::PACK_NOT_APPLICABLE.to_owned(),
            key: p.key(),
            message: format!(
                "{} ({}) matches none of this job's knowledge kinds; it is listed, not used.",
                p.pin(),
                p.layer
            ),
        });
    }
    if mode == Mode::Preview && selection.is_base_only_preview(job) {
        warnings.push(Warning {
            code: code::BASE_ONLY_PREVIEW.to_owned(),
            key: String::new(),
            message:
                "Base model only: a preview, not a job (a job needs an industry and a department)."
                    .to_owned(),
        });
    }

    let job_fields = job.and_then(|j| {
        overlay[JOB].get(j).map(|e| {
            let mut m = Map::new();
            m.insert("key".to_owned(), Value::String(j.to_owned()));
            m.extend(e.fields.clone());
            m
        })
    });
    let job_content = if job.is_some() {
        JobContent {
            context_spaces,
            procedures: overlay[PROCEDURE].to_items(),
            connectors: overlay[CONNECTOR].to_items(),
            knowledge_stores: overlay[KNOWLEDGE_STORE].to_items(),
        }
    } else {
        JobContent {
            context_spaces,
            procedures: Items::new(),
            connectors: Items::new(),
            knowledge_stores: overlay[KNOWLEDGE_STORE].to_items(),
        }
    };

    let mut effective = EffectiveConfig {
        hash: String::new(),
        entities: overlay[ENTITY].to_items(),
        attributes: overlay[ATTRIBUTE].to_items(),
        relations: overlay[RELATION].to_items(),
        job: job_fields,
        job_content,
        provenance,
        warnings,
        layers: chosen.iter().map(|l| l.label()).collect(),
        rules_fired: fired.iter().map(|r| r.key.clone()).collect(),
    };
    effective.hash = effective_hash(&effective);

    let [
        objects,
        _folded_operations,
        error_states,
        gates,
        agent_roles,
        job_settings,
        rule_slots,
    ] = catalogue;
    let mut gates = gates.into_ordered();
    generate_activity_options(&mut gates, &knowledge.activities);
    let mut agent_roles = agent_roles.into_ordered();
    for role in &mut agent_roles {
        derive_grants(role, &expectations, &operations);
    }
    Ok(Compiled {
        effective,
        catalogue: CompiledCatalogue {
            objects: objects.into_ordered(),
            operations,
            error_states: error_states.into_ordered(),
            gates,
            agent_roles,
            expectations,
            job_settings: job_settings.into_ordered(),
            rule_slots: rule_slots.into_ordered(),
            instructions: instructions.finish(),
        },
        knowledge,
        provenance_rows,
    })
}

#[cfg(test)]
mod selection_tests {
    use super::*;

    /// KH-F3: a stored `departments: null` (or `industry: null`) reads as empty, so it is refused
    /// `hierarchy_incomplete` — never a deserialisation error (a reader's 500).
    #[test]
    fn null_departments_read_as_none_and_are_refused_hierarchy_incomplete() {
        let sel: Selection = serde_json::from_value(serde_json::json!({
            "profile": "retail_ecommerce", "industry": null, "departments": null,
        }))
        .expect("null reads as empty");
        assert!(sel.industry.is_empty() && sel.departments.is_empty());
        let issues: Vec<(String, String)> = sel
            .hierarchy_issues()
            .into_iter()
            .map(|i| (i.code, i.path))
            .collect();
        assert_eq!(
            issues,
            [
                (
                    code::HIERARCHY_INCOMPLETE.to_owned(),
                    "selection.industry".to_owned()
                ),
                (
                    code::HIERARCHY_INCOMPLETE.to_owned(),
                    "selection.departments".to_owned()
                ),
            ]
        );
        // a list still reads as the list; another shape is still a shape error
        let sel: Selection = serde_json::from_value(serde_json::json!({
            "industry": "retail", "departments": ["industry_dept/retail/customer_service"],
        }))
        .unwrap();
        assert_eq!(sel.departments, ["industry_dept/retail/customer_service"]);
        assert!(
            serde_json::from_value::<Selection>(serde_json::json!({"departments": "x"})).is_err()
        );
    }
}

/// D116: the base-department layer `base_dept/<a>` folds at the archetype position of each department extending `<a>`.
#[cfg(test)]
mod base_dept_tests {
    use super::*;
    use serde_json::json;

    const CS: &str = "industry_dept/retail/customer_service";
    const BILLING: &str = "industry_dept/retail/billing";

    fn layer(id: &str, label: &str, extends: Option<&str>) -> LayerMeta {
        LayerMeta {
            id: id.to_owned(),
            key: LayerKey::parse_label(label).unwrap_or_else(|e| panic!("{label}: {e}")),
            extends: extends.map(str::to_owned),
            tag: None,
            tagged_at: None,
        }
    }

    fn row(id: &str, layer_id: &str, kind: Kind, key: &str, op: Op, fields: Value) -> Row {
        row_rev(id, layer_id, kind, key, op, fields, 1)
    }

    fn row_rev(
        id: &str,
        layer_id: &str,
        kind: Kind,
        key: &str,
        op: Op,
        fields: Value,
        rev: i32,
    ) -> Row {
        Row {
            id: id.to_owned(),
            layer_id: layer_id.to_owned(),
            kind,
            key: key.to_owned(),
            op,
            fields,
            rev,
        }
    }

    fn layers() -> Vec<LayerMeta> {
        vec![
            layer("L-base", "base", None),
            layer("L-ret", "industry/retail", None),
            layer("L-bd", "base_dept/customer_service", None),
            layer("L-bd-nl", "base_dept/customer_service/region/nl", None),
            layer("L-cs", CS, Some("customer_service")),
            layer("L-job", "job/care", None),
        ]
    }

    fn selection(departments: &[&str], region: Option<&str>) -> Selection {
        Selection {
            industry: "retail".to_owned(),
            departments: departments.iter().map(|d| (*d).to_owned()).collect(),
            region: region.map(str::to_owned),
            ..Selection::default()
        }
    }

    fn laid_out(steps: &[FoldStep<'_>]) -> Vec<(FoldPosition, String, Option<usize>)> {
        steps
            .iter()
            .map(|s| (s.position, s.label(), s.department))
            .collect()
    }

    #[test]
    fn fold_order_puts_base_dept_at_the_archetype_position() {
        use FoldPosition as P;
        assert!(FOLD_ORDER.windows(2).all(|w| w[0] < w[1]));
        let ls = layers();
        let steps = fold_order(&ls, &selection(&[CS], None), Some("care"), &[]);
        assert_eq!(
            laid_out(&steps),
            vec![
                (P::Base, "base".to_owned(), None),
                (P::Industry, "industry/retail".to_owned(), None),
                (
                    P::Archetype,
                    "department_archetype/customer_service".to_owned(),
                    Some(0)
                ),
                (
                    P::Archetype,
                    "base_dept/customer_service".to_owned(),
                    Some(0)
                ),
                (P::Department, CS.to_owned(), Some(0)),
                (P::Job, "job/care".to_owned(), None),
            ]
        );
        assert!(matches!(
            steps[2].source,
            FoldSource::Archetype("customer_service")
        ));
        assert_eq!(steps[2].archetype(), Some("customer_service"));
        assert_eq!(steps[3].archetype(), Some("customer_service"));
        assert_eq!(steps[4].archetype(), None);
        assert_eq!(steps[0].archetype(), None);

        // the selection's region adds the base_dept region sub-layer right after its parent
        let steps = fold_order(&ls, &selection(&[CS], Some("nl")), Some("care"), &[]);
        assert_eq!(
            laid_out(&steps),
            vec![
                (P::Base, "base".to_owned(), None),
                (P::Industry, "industry/retail".to_owned(), None),
                (
                    P::Archetype,
                    "department_archetype/customer_service".to_owned(),
                    Some(0)
                ),
                (
                    P::Archetype,
                    "base_dept/customer_service".to_owned(),
                    Some(0)
                ),
                (
                    P::ArchetypeRegion,
                    "base_dept/customer_service/region/nl".to_owned(),
                    Some(0)
                ),
                (P::Department, CS.to_owned(), Some(0)),
                (P::Job, "job/care".to_owned(), None),
            ]
        );
        assert_eq!(steps[4].archetype(), Some("customer_service"));
        // another region names no sub-layer
        let other = fold_order(&ls, &selection(&[CS], Some("de")), Some("care"), &[]);
        assert_eq!(other.len(), 6);
    }

    #[test]
    fn base_dept_folds_once_for_departments_sharing_the_archetype() {
        use FoldPosition as P;
        let mut ls = layers();
        ls.push(layer("L-bill", BILLING, Some("customer_service")));
        let steps = fold_order(&ls, &selection(&[CS, BILLING], None), None, &[]);
        let at_archetype: Vec<(FoldPosition, String, Option<usize>)> = laid_out(&steps)
            .into_iter()
            .filter(|(p, _, _)| *p == P::Archetype)
            .collect();
        assert_eq!(
            at_archetype,
            vec![
                (
                    P::Archetype,
                    "department_archetype/customer_service".to_owned(),
                    Some(0)
                ),
                (
                    P::Archetype,
                    "base_dept/customer_service".to_owned(),
                    Some(0)
                ),
            ]
        );
        // its knowledge belongs to every department extending it, and gives way as an archetype's does
        let rows = vec![
            row(
                "r-job",
                "L-base",
                Kind::Job,
                "care",
                Op::Add,
                json!({"label": "Care"}),
            ),
            row(
                "k-bd",
                "L-bd",
                Kind::StageKnowledgeKind,
                "*|cs.returns",
                Op::Add,
                json!({"label": "Returns"}),
            ),
            row(
                "k-bill",
                "L-bill",
                Kind::StageKnowledgeKind,
                "support|cs.returns",
                Op::Add,
                json!({"label": "Returns at support"}),
            ),
        ];
        let (knowledge, errors) =
            fold_knowledge(&ls, &rows, &selection(&[CS, BILLING], None), None);
        assert!(errors.is_empty(), "{errors:#?}");
        let any = knowledge
            .kinds
            .iter()
            .find(|k| k.key() == "*|cs.returns")
            .expect("the base_dept kind folds");
        assert_eq!(any.layer, "base_dept/customer_service");
        // billing places the kind at a stage, so the base_dept `*` kind gives way for billing only
        assert_eq!(any.departments, vec![CS.to_owned()]);
    }

    #[test]
    fn two_base_depts_that_differ_are_a_sibling_conflict() {
        let ls = vec![
            layer("L-base", "base", None),
            layer("L-ret", "industry/retail", None),
            layer("L-bd-cs", "base_dept/customer_service", None),
            layer("L-bd-bill", "base_dept/billing", None),
            layer("L-cs", CS, Some("customer_service")),
            layer("L-bill", BILLING, Some("billing")),
        ];
        let rows = vec![
            row(
                "p-cs",
                "L-bd-cs",
                Kind::Procedure,
                "verify_identity@1",
                Op::Add,
                json!({"label": "Verify (service)"}),
            ),
            row(
                "p-bill",
                "L-bd-bill",
                Kind::Procedure,
                "verify_identity@1",
                Op::Add,
                json!({"label": "Verify (billing)"}),
            ),
        ];
        let steps = fold_order(&ls, &selection(&[CS, BILLING], None), None, &[]);
        let conflicts = department_conflicts(&steps, &rows);
        assert_eq!(
            conflicts,
            vec![DepartmentConflict {
                kind: Kind::Procedure,
                key: "verify_identity@1".to_owned(),
                layers: vec![
                    "base_dept/customer_service".to_owned(),
                    "base_dept/billing".to_owned()
                ],
                resolved: false,
            }]
        );
        // equal values are no conflict
        let mut same = rows.clone();
        same[1].fields = same[0].fields.clone();
        assert!(department_conflicts(&steps, &same).is_empty());
    }

    fn gate() -> Value {
        json!({
            "group": "base",
            "point": "P1",
            "type": "choice",
            "question": "Which theme is the customer in?",
            "options": [
                {"key": "need", "label": "Need", "description": "", "generated": false},
                {"key": "issue", "label": "Issue", "description": "", "generated": false}
            ],
            "generated_from": null,
            "threshold": "top ≥ 0.5",
            "threshold_value": 0.5,
            "fail": "open",
            "safety": false,
            "effect": "Tone; stage prior.",
            "rule": {"fire_when": "in", "threshold": null, "fire_values": ["issue"], "min_confidence": 0.5},
            "action": "annotate",
            "tools": null,
            "origin": "platform",
            "updated_by": null,
            "updated_at": "2026-10-12T09:00:00Z"
        })
    }

    /// The activity gate GA offers the job's own activities (`stage:activity`), never the library row's static
    /// options: a base row that still lists a retail label does not reach a job without that activity.
    #[test]
    fn the_activity_gate_offers_the_jobs_own_activities() {
        let ls = layers();
        let mut ga = gate();
        ga["question"] = json!("Which activity is the customer doing?");
        ga["options"] = json!([
            {"key": "delivery:track_parcel", "label": "Track a parcel", "description": "", "generated": true}
        ]);
        ga["generated_from"] = json!("the activities of the job");
        ga["threshold_value"] = json!(0.7);
        ga["rule"] = json!({"fire_when": "gte", "threshold": 0.7, "fire_values": null, "min_confidence": null});
        let rows = vec![
            row(
                "r-job",
                "L-base",
                Kind::Job,
                "care",
                Op::Add,
                json!({"label": "Care"}),
            ),
            row("g-ga", "L-base", Kind::GateQuestion, "GA", Op::Add, ga),
            row("g-gd", "L-bd", Kind::GateQuestion, "GD", Op::Add, gate()),
            row(
                "a-bd",
                "L-bd",
                Kind::Activity,
                "support:track_request",
                Op::Add,
                json!({"label": "Track a request"}),
            ),
            row(
                "a-cs",
                "L-cs",
                Kind::Activity,
                "change:update_details",
                Op::Add,
                json!({"label": "Update my details", "summary": "employee · contact_point.update", "description": "Change my email, phone or address", "actor": "employee"}),
            ),
        ];
        let departments = DepartmentRef::from_layers(&ls);
        let input = CompileInput {
            layers: &ls,
            rows: &rows,
            rules: &[],
            profiles: &[],
            departments: &departments,
        };
        let compiled = compile(&input, &selection(&[CS], None), Some("care"))
            .unwrap_or_else(|e| panic!("unexpected compile errors: {e:#?}"));
        let option_keys = |key: &str| -> Vec<String> {
            compiled
                .catalogue
                .gates
                .iter()
                .find(|g| g.key == key)
                .unwrap_or_else(|| panic!("gate {key} is compiled"))
                .fields["options"]
                .as_array()
                .unwrap()
                .iter()
                .map(|o| o["key"].as_str().unwrap().to_owned())
                .collect()
        };
        // exactly the folded activities, in key order; the retail label of the row is gone
        assert_eq!(
            option_keys(ACTIVITY_GATE),
            ["change:update_details", "support:track_request"]
        );
        let ga = compiled
            .catalogue
            .gates
            .iter()
            .find(|g| g.key == ACTIVITY_GATE)
            .unwrap();
        assert_eq!(ga.fields["options"][0]["label"], json!("Update my details"));
        assert_eq!(ga.fields["options"][0]["generated"], json!(true));
        // a generated option describes itself: the activity's plain description, or its label when the row has none
        assert_eq!(
            ga.fields["options"][0]["description"],
            json!("Change my email, phone or address")
        );
        assert_eq!(
            ga.fields["options"][1]["description"],
            json!("Track a request")
        );
        let folded: Vec<&str> = compiled
            .knowledge
            .activities
            .iter()
            .map(|a| a.key.as_str())
            .collect();
        assert_eq!(folded, ["change:update_details", "support:track_request"]);
        // every other gate keeps its own options
        assert_eq!(option_keys("GD"), ["need", "issue"]);
    }

    #[test]
    fn base_dept_content_reaches_the_job_and_the_department_overrides_it() {
        let ls = layers();
        let mut rows = vec![
            row(
                "r-job",
                "L-base",
                Kind::Job,
                "care",
                Op::Add,
                json!({"label": "Care"}),
            ),
            row(
                "p-bd",
                "L-bd",
                Kind::Procedure,
                "verify_identity@1",
                Op::Add,
                json!({"label": "Verify identity", "uses_slots": ["crm"]}),
            ),
            row(
                "s-bd",
                "L-bd",
                Kind::ConnectorSlot,
                "crm",
                Op::Add,
                json!({"label": "CRM"}),
            ),
            row("g-bd", "L-bd", Kind::GateQuestion, "GD", Op::Add, gate()),
        ];
        let departments = DepartmentRef::from_layers(&ls);
        let compile_rows = |rows: &[Row]| -> Compiled {
            let input = CompileInput {
                layers: &ls,
                rows,
                rules: &[],
                profiles: &[],
                departments: &departments,
            };
            compile(&input, &selection(&[CS], None), Some("care"))
                .unwrap_or_else(|e| panic!("unexpected compile errors: {e:#?}"))
        };

        let inherited = compile_rows(&rows);
        assert_eq!(
            inherited.effective.layers,
            vec![
                "base",
                "industry/retail",
                "base_dept/customer_service",
                CS,
                "job/care"
            ]
        );
        let content = &inherited.effective.job_content;
        assert_eq!(
            content.procedures["verify_identity@1"]["label"],
            json!("Verify identity")
        );
        assert!(content.connectors.contains_key("crm"));
        let gates: Vec<(&str, &str)> = inherited
            .catalogue
            .gates
            .iter()
            .map(|g| (g.key.as_str(), g.layer.as_str()))
            .collect();
        assert_eq!(gates, vec![("GD", "base_dept/customer_service")]);

        // the department's row of the same key overrides the base_dept one
        rows.push(row(
            "p-cs",
            "L-cs",
            Kind::Procedure,
            "verify_identity@1",
            Op::Patch,
            json!({"label": "Verify identity (retail)"}),
        ));
        let overridden = compile_rows(&rows);
        assert_eq!(
            overridden.effective.job_content.procedures["verify_identity@1"]["label"],
            json!("Verify identity (retail)")
        );
        assert_eq!(
            overridden.effective.job_content.procedures["verify_identity@1"]["uses_slots"],
            json!(["crm"])
        );
    }

    /// The authoring API stores an in-place edit of a layer's own row as a `patch` at the next revision. With no
    /// earlier layer adding the key, that patch IS the row's definition — this is how base's `agent_role assistant`
    /// is stored since 2026-11-01, and reading it as a patch with no target made every head compile fail.
    /// A first-revision patch still names a target nothing adds (a typo, not an edit).
    #[test]
    fn a_later_revision_patch_defines_the_layers_own_row() {
        let ls = layers();
        let departments = DepartmentRef::from_layers(&ls);
        let role = json!({"enabled": true, "grants": [], "label": "Assistant",
                          "max_side_effect": "W", "used_by": "interaction"});
        let rows = vec![row_rev(
            "role-base",
            "L-base",
            Kind::AgentRole,
            "assistant",
            Op::Patch,
            role.clone(),
            2,
        )];
        let input = CompileInput {
            layers: &ls,
            rows: &rows,
            rules: &[],
            profiles: &[],
            departments: &departments,
        };
        let compiled = compile(&input, &selection(&[CS], None), None)
            .unwrap_or_else(|e| panic!("a self-edit defines the row: {e:#?}"));
        let assistant = compiled
            .catalogue
            .agent_roles
            .iter()
            .find(|r| r.key == "assistant")
            .unwrap_or_else(|| panic!("no assistant role in {:?}", compiled.catalogue.agent_roles));
        assert_eq!(assistant.fields["label"], json!("Assistant"));

        // a first revision cannot be an edit of something this layer no longer holds
        let rows = vec![row_rev(
            "role-base",
            "L-base",
            Kind::AgentRole,
            "assistant",
            Op::Patch,
            role,
            1,
        )];
        let input = CompileInput {
            layers: &ls,
            rows: &rows,
            rules: &[],
            profiles: &[],
            departments: &departments,
        };
        let errors = compile(&input, &selection(&[CS], None), None)
            .expect_err("a rev-1 patch names no target");
        assert!(
            errors.iter().any(|e| e.code == code::PATCH_TARGET_MISSING),
            "{errors:#?}"
        );
    }

    #[test]
    fn base_dept_refuses_kinds_outside_its_scopes() {
        let ls = layers();
        let rows = vec![
            row(
                "r-job",
                "L-base",
                Kind::Job,
                "care",
                Op::Add,
                json!({"label": "Care"}),
            ),
            row(
                "j-bd",
                "L-bd",
                Kind::Job,
                "other",
                Op::Add,
                json!({"label": "Other"}),
            ),
        ];
        let departments = DepartmentRef::from_layers(&ls);
        let input = CompileInput {
            layers: &ls,
            rows: &rows,
            rules: &[],
            profiles: &[],
            departments: &departments,
        };
        let errors = compile(&input, &selection(&[CS], None), Some("care"))
            .expect_err("a job row is not allowed in base_dept");
        assert!(
            errors
                .iter()
                .any(|e| e.code == code::KIND_NOT_ALLOWED_IN_LAYER),
            "{errors:#?}"
        );
    }
}
