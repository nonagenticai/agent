//! The template compiler seam (spec B2 §5): what approve (T11) and apply (T12) call to turn a job's selection and
//! template pins into a release [`Bundle`], plus the selection checks (T7) and the pin adoption of an approve
//! (`adopt_pins`, B6 §4).
//!
//! [`LayerCompiler`] (`AppState.compiler`) implements [`TemplateCompiler`]: B6a's pinned compile
//! (`platform::templates::compile_pinned`) over the platform's DB rows. There is no compiled-in content: a bundle is
//! the platform's rows folded for the selection, plus the parts [`derive`] computes from that fold.
//!
//! # Where each part of a bundle comes from
//! | Bundle part | Source |
//! |---|---|
//! | `objects.activity` | the KH fold of the pinned `activity` rows (R2, [`activity_items`]): the same fold is the one source of the activities' operations |
//! | `objects.context_space` | B6a `effective.job_content.context_spaces`, fixture shape ([`context_space_item`]) |
//! | `objects.ontology_attribute` | B6a `effective.attributes`, fixture shape ([`attribute_item`]) |
//! | `objects.knowledge_store` | B6a `effective.job_content.knowledge_stores`, fixture shape ([`knowledge_store_item`]) |
//! | `objects.gate_option` | B6a `catalogue.gates`: every option with `generated: true`, fixture shape ([`gate_options`]) |
//! | `settings` (`preamble`, `actor`, `stance`) | [`settings_of`] the B6a compile: `preamble` = the folded `identity` instruction (D108), `actor` / `stance` = the nearest folded `job_setting` value (a `job` row's `actor` / `stance` folds as its layer's setting) |
//! | `objects.knowledge_pack` | the `knowledge_pack` fold of the pinned tags per seeded knowledge kind ([`knowledge_pack_items`], PK-2) |
//! | `objects.instruction` | B6a `catalogue.instructions`, the folded prompt slots (D108, KH T4) |
//! | `profile_values` | the selection's `profile` row from the exact pinned tags (`platform::db::profiles`), each axis' values joined with ` + ` |
//! | `requires`, `causes`, violations | [`derive::derive`] from the objects and the selection |
//! | `rules` | the folded `rule_slot` rows of the pinned layers ([`rule_slot_items`], 0222): key = rule id, `slot_layer` defaults to the layer that sets the slot; empty when no layer authors one |
//!
//! `sections` are rebuilt from those parts ([`sync_sections`]):
//! `activities` = `<key> — <label>` per `objects.activity` item; `context_spaces` = the `objects.context_space` keys; `gates` = the compiled gates
//! (`"<key> (closed)"` for a gate that fails closed, the fixture's notation); `knowledge_kinds` = the kinds of the
//! knowledge stores, then of the context spaces, first seen first; `settings` = the settings keys present, in the
//! fixture's order `actor`, `stance`, `preamble`; `channels` and `regions` echo the selection. A section is added only when it has content (`channels` and
//! `regions` always).
//!
//! # Fixture item shapes
//! The fixture `OverlayItem` shapes are the contract T8 refines and the web renders, so B6a's field names are mapped
//! onto them (renames only; nothing is invented beyond empty defaults):
//! - context space: `slots` → `required`; `required` and `entities` default to `[]`, `label` to the key; the other
//!   fields (`area`, `knowledge_kinds`, …) pass through;
//! - ontology attribute: `data_type` → `type`; `label` defaults to the key; `pii`, `values`, `note`, … pass through;
//! - knowledge store: `label` defaults to the key; `kind` = the family of the first of `kinds` (the part before
//!   the first `.`, e.g. `policy.returns` → `policy`; omitted when `kinds` is empty), and `kinds` passes through;
//! - gate option: `{key: "<gate> · <option>", label, generated}`, plus `description` when non-empty.
//!
//! B6a is compiled first: its refusals keep their status and code (404 `tag_not_found`, 409 `template_not_tagged`,
//! 404 `profile_not_found`), its compile errors become [`CompileError::Failed`] (422 `compile_failed`). The job layer
//! compiled is the first `job/*` pin's job (`job/customer_care_agent` → `customer_care_agent`); a pin set without a
//! `job/*` pin compiles without a job row (no settings, no procedures).
//!
//! # Cache
//! A successful [`LayerCompiler`] compile is cached in-process (LRU, [`BUNDLE_CACHE_CAP`]), keyed by the
//! canonical JSON of `{selection, pins, revision}`. Arrays keep their order (the bundle echoes the selection's
//! channel and region order). `revision` = [`COMPILER_REVISION`] + the platform store's row, layer and tag counts:
//! rows are append-only and tags immutable, so any store change that can alter a compile changes the key.
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, Mutex, PoisonError};

use agent_model::compile::{self as model, Items, Selection};
use axum::http::StatusCode;
use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};
use sqlx::PgConnection;

use agent_model::kinds::instruction::{IDENTITY, slot_text};

use super::bundle::{
    BUNDLE_FORMAT, Bundle, BundleObjects, BundleSection, BundleSettings, BundleViolation,
    OverlayItem, Requires, RuleSlot, SelectionChoice, SelectionDimension,
    SelectionOptionDimensions, SelectionOptions, instruction_items,
};
use super::{derive, hash};
use crate::design::JobRef;
use crate::error::{ApiError, ApiResult};
use crate::job_design::base::{ActivityOp, derived_ops};
use crate::knowledge::kinds;
use crate::knowledge::packs::template as template_packs;
use crate::platform::db::profiles as db_profiles;
use crate::platform::db::tags as db_tags;
use crate::platform::db::vocab;
use crate::platform::templates::{self, TemplatePin};

/// Bump when [`LayerCompiler`] maps B6a output differently (part of the cache key).
pub const COMPILER_REVISION: &str = "layer/5";
/// The bundle cache capacity.
pub const BUNDLE_CACHE_CAP: usize = 64;

/// A compiled bundle: the bundle, its hash, and what keeps it from being approved (fixture `violationsFor`; 422
/// `bundle_incomplete` when non-empty).
#[derive(Debug, Clone, PartialEq)]
pub struct Compiled {
    pub bundle: Bundle,
    pub bundle_hash: String,
    pub violations: Vec<BundleViolation>,
}

/// Why a compile produced no bundle.
#[derive(Debug)]
pub enum CompileError {
    /// The template does not compile: 422 `compile_failed` with `errors[{path, message}]`.
    Failed(Vec<BundleViolation>),
    /// A refusal that keeps its own status and code (B6a's 404 `tag_not_found` / `profile_not_found`, 409
    /// `template_not_tagged`), or a database error. Boxed: an `ApiError` is large.
    Api(Box<ApiError>),
}

impl From<ApiError> for CompileError {
    fn from(e: ApiError) -> Self {
        CompileError::Api(Box::new(e))
    }
}

impl From<CompileError> for ApiError {
    fn from(e: CompileError) -> Self {
        match e {
            CompileError::Failed(errors) => compile_failed(&errors),
            CompileError::Api(e) => *e,
        }
    }
}

fn problems(n: usize) -> String {
    format!("{n} problem{}", if n == 1 { "" } else { "s" })
}

fn violations_json(list: &[BundleViolation]) -> Value {
    Value::Array(
        list.iter()
            .map(|v| json!({"path": v.path, "message": v.message}))
            .collect(),
    )
}

/// 422 `compile_failed` with `errors[{path, message}]`.
pub fn compile_failed(errors: &[BundleViolation]) -> ApiError {
    ApiError::detailed(
        StatusCode::UNPROCESSABLE_ENTITY,
        "compile_failed",
        format!("The template does not compile: {}.", problems(errors.len())),
    )
    .with("errors", violations_json(errors))
}

/// 422 `bundle_incomplete` with `violations[{path, message}]` (fixture `approveRelease` text).
pub fn bundle_incomplete(violations: &[BundleViolation]) -> ApiError {
    ApiError::detailed(
        StatusCode::UNPROCESSABLE_ENTITY,
        "bundle_incomplete",
        format!(
            "The bundle has {}; nothing was approved.",
            problems(violations.len())
        ),
    )
    .with("violations", violations_json(violations))
}

/// The compile seam (spec B2 §5). [`LayerCompiler`] implements it.
pub trait TemplateCompiler: Send + Sync + 'static {
    /// Compile `sel` over `pins` into a bundle.
    fn compile<'a>(
        &'a self,
        c: &'a mut PgConnection,
        sel: &'a Selection,
        pins: &'a [TemplatePin],
    ) -> BoxFuture<'a, Result<Compiled, CompileError>>;
    /// What a selection may name (D65).
    fn options<'a>(&'a self, c: &'a mut PgConnection)
    -> BoxFuture<'a, ApiResult<SelectionOptions>>;
    /// The newest tag per family (`adopt_pins`, release status `newer_pins`).
    fn latest_tags<'a>(
        &'a self,
        c: &'a mut PgConnection,
    ) -> BoxFuture<'a, ApiResult<BTreeMap<String, i32>>>;
    /// The families (and versions) the tag `pin` was cut on top of (B6 §4: adopting a tag adopts its stack).
    /// Default: none.
    fn stack<'a>(
        &'a self,
        _c: &'a mut PgConnection,
        _pin: &'a TemplatePin,
    ) -> BoxFuture<'a, ApiResult<Vec<TemplatePin>>> {
        Box::pin(async { Ok(Vec::new()) })
    }
}

// ---- pins adopted (§2.7) -----------------------------------------------------------------------------------------

/// Run by approve after the compile (step 5) and before the snapshot (step 6), when the cut adopts new pins
/// (B5 re-materialises its base rows). Registered on `AppState.pins_adopted`.
pub trait PinsAdopted: Send + Sync + 'static {
    fn on_pins_adopted<'a>(
        &'a self,
        c: &'a mut PgConnection,
        job: &'a JobRef,
        old: &'a [TemplatePin],
        new: &'a [TemplatePin],
        compiled: &'a Compiled,
    ) -> BoxFuture<'a, ApiResult<()>>;
}

/// Run every hook in registration order, in the caller's transaction; the first error aborts. Nothing runs when
/// `old == new` (no pin was adopted).
pub async fn run_pins_adopted(
    hooks: &[Arc<dyn PinsAdopted>],
    c: &mut PgConnection,
    job: &JobRef,
    old: &[TemplatePin],
    new: &[TemplatePin],
    compiled: &Compiled,
) -> ApiResult<()> {
    if old == new {
        return Ok(());
    }
    for h in hooks {
        h.on_pins_adopted(&mut *c, job, old, new, compiled).await?;
    }
    Ok(())
}

/// The pins a cut compiles (fixture `adoptedPins` + B6 §4): every family in `adopt` moves to its newest tag
/// (`compiler.latest_tags`), and each adopted tag brings the stack it was cut on (a stack entry raises a lower pin
/// of its family or adds a missing family; it never lowers a pin). Refusals: 422 `unknown_layer` (the job does not
/// pin it), 409 `nothing_to_adopt` (already the newest tag). Empty `adopt` returns `pins` unchanged.
pub async fn adopted_pins(
    c: &mut PgConnection,
    compiler: &dyn TemplateCompiler,
    pins: &[TemplatePin],
    adopt: &[String],
) -> ApiResult<Vec<TemplatePin>> {
    let mut want: Vec<&str> = Vec::new();
    for l in adopt {
        if !want.contains(&l.as_str()) {
            want.push(l.as_str());
        }
    }
    if want.is_empty() {
        return Ok(pins.to_vec());
    }
    let latest = compiler.latest_tags(&mut *c).await?;
    for l in &want {
        let Some(pin) = pins.iter().find(|p| p.layer == *l) else {
            return Err(ApiError::Unprocessable(
                "unknown_layer",
                format!("This job does not pin {l}."),
            ));
        };
        if latest.get(*l).copied().unwrap_or(pin.version) <= pin.version {
            return Err(ApiError::Conflict(
                "nothing_to_adopt",
                format!("{l}@v{} is already the newest tag.", pin.version),
            ));
        }
    }
    let mut out: Vec<TemplatePin> = pins
        .iter()
        .map(|p| {
            let version = if want.contains(&p.layer.as_str()) {
                latest.get(&p.layer).copied().unwrap_or(p.version)
            } else {
                p.version
            };
            TemplatePin {
                layer: p.layer.clone(),
                version,
            }
        })
        .collect();
    let adopted: Vec<TemplatePin> = out
        .iter()
        .filter(|p| want.contains(&p.layer.as_str()))
        .cloned()
        .collect();
    for a in &adopted {
        for s in compiler.stack(&mut *c, a).await? {
            match out.iter_mut().find(|p| p.layer == s.layer) {
                Some(p) => p.version = p.version.max(s.version),
                None => out.push(s),
            }
        }
    }
    Ok(out)
}

// ---- selection checks (fixture `invalidSelection` / `selectionError`) --------------------------------------------

/// A null selection field as the fixture's template string shows it.
fn shown(v: &Option<String>) -> &str {
    v.as_deref().unwrap_or("null")
}

fn unknown<'s>(values: &'s [String], known: &[SelectionChoice]) -> Vec<&'s str> {
    values
        .iter()
        .filter(|v| !known.iter().any(|x| x.key == **v))
        .map(String::as_str)
        .collect()
}

/// What makes a stored selection stale (fixture `selectionError`; `selection_error` of the target and the release
/// status). `None` for no selection or a selection the options still cover.
pub fn selection_error(o: &SelectionOptions, s: Option<&Selection>) -> Option<String> {
    let s = s?;
    if !o
        .profiles
        .iter()
        .any(|p| Some(p.key.as_str()) == s.profile.as_deref())
    {
        return Some(format!("Profile “{}” no longer exists.", shown(&s.profile)));
    }
    let bad_area = unknown(&s.areas, &o.areas);
    if !bad_area.is_empty() {
        return Some(format!(
            "Areas that no longer exist: {}.",
            bad_area.join(", ")
        ));
    }
    for (key, values, dim) in [
        ("channel", s.values("channel"), &o.dimensions.channel),
        ("region", s.values("region"), &o.dimensions.region),
    ] {
        let bad = unknown(values, &dim.values);
        if !bad.is_empty() {
            return Some(format!(
                "Dimension {key} names values that no longer exist: {}.",
                bad.join(", ")
            ));
        }
    }
    None
}

/// Why a selection may not be saved (fixture `invalidSelectionIn`), else `None`: the D106/D115 hierarchy first
/// ([`Selection::hierarchy_issues`], its first message), then the options.
pub fn invalid_selection(o: &SelectionOptions, s: &Selection) -> Option<String> {
    if let Some(issue) = s.hierarchy_issues().into_iter().next() {
        return Some(issue.message);
    }
    if !o
        .profiles
        .iter()
        .any(|p| Some(p.key.as_str()) == s.profile.as_deref())
    {
        return Some(format!("Unknown profile “{}”.", shown(&s.profile)));
    }
    if !o.industries.iter().any(|p| p.key == s.industry) {
        return Some(format!("Unknown industry “{}”.", s.industry));
    }
    let depts: Vec<SelectionChoice> = o
        .departments
        .iter()
        .filter(|x| x.industry.as_deref() == Some(s.industry.as_str()))
        .cloned()
        .collect();
    let bad_dept = unknown(&s.departments, &depts);
    if !bad_dept.is_empty() {
        return Some(format!(
            "{} has no department {}.",
            s.industry,
            bad_dept.join(", ")
        ));
    }
    let bad_area: Vec<&str> = s
        .areas
        .iter()
        .filter(|a| {
            !o.areas.iter().any(|x| {
                x.key == **a
                    && s.departments
                        .iter()
                        .any(|d| d.as_str() == x.industry_dept.as_deref().unwrap_or(""))
            })
        })
        .map(String::as_str)
        .collect();
    if !bad_area.is_empty() {
        return Some(format!(
            "Areas outside the chosen departments: {}.",
            bad_area.join(", ")
        ));
    }
    selection_error(o, Some(s))
}

/// [`invalid_selection`] as the save's refusal: a broken hierarchy is 422 under its own code
/// ([`hierarchy_refusal`]), anything else 422 `invalid_selection` with the fixture's message.
pub fn check_selection(o: &SelectionOptions, s: &Selection) -> ApiResult<()> {
    templates::hierarchy_refusal(s)?;
    match invalid_selection(o, s) {
        Some(message) => Err(ApiError::Unprocessable("invalid_selection", message)),
        None => Ok(()),
    }
}

/// The version of the first `job/*` pin, else 0 (fixture `jobTemplateVersion`).
pub fn job_template_version(pins: &[TemplatePin]) -> i32 {
    pins.iter()
        .find(|p| p.layer.starts_with("job/"))
        .map_or(0, |p| p.version)
}

/// The job whose layer the compile applies: the first `job/*` pin's job (`job/customer_care_agent` →
/// `customer_care_agent`).
pub fn job_key(pins: &[TemplatePin]) -> Option<&str> {
    pins.iter()
        .find(|p| p.layer.starts_with("job/"))
        .and_then(|p| p.layer.strip_prefix("job/"))
}

fn failed(path: impl Into<String>, message: impl Into<String>) -> CompileError {
    CompileError::Failed(vec![BundleViolation {
        path: path.into(),
        message: message.into(),
    }])
}

/// Hash the bundle. A value the hash cannot take (a float) is a compile failure at its path.
fn finish(bundle: Bundle, violations: Vec<BundleViolation>) -> Result<Compiled, CompileError> {
    let bundle_hash = bundle.hash().map_err(|e| {
        failed(
            format!("bundle{}", e.path()),
            format!("The bundle cannot be hashed: {e}."),
        )
    })?;
    Ok(Compiled {
        bundle,
        bundle_hash,
        violations,
    })
}

/// 0222: the folded `rule_slot` items as bundle rule slots. The item's key is the rule id; `slot_layer` is the row's
/// own value, else the layer that set the slot. An item that does not read as a [`RuleSlot`] fails the compile at its
/// path (the kind's validation makes that unreachable for a stored row).
pub(crate) fn rule_slot_items(
    items: &[model::CatalogueItem],
) -> Result<Vec<RuleSlot>, CompileError> {
    items
        .iter()
        .map(|it| {
            let mut m = it.fields.clone();
            m.remove("summary");
            m.insert("id".to_owned(), Value::String(it.key.clone()));
            if m.get("slot_layer").is_none_or(Value::is_null) {
                m.insert("slot_layer".to_owned(), Value::String(it.layer.clone()));
            }
            serde_json::from_value::<RuleSlot>(Value::Object(m)).map_err(|e| {
                failed(
                    format!("rules.{}", it.key),
                    format!("rule_slot {} is unreadable: {e}.", it.key),
                )
            })
        })
        .collect()
}

/// One `layer` head row read by [`db_options`]: key, scope, industry, department, label, extends, areas.
type LayerHead = (
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<Vec<String>>,
);

/// D128: what a selection may name, read from the platform's DB. Industries and departments are the layer heads
/// (`industry/<i>`, `industry_dept/<i>/<d>`) with at least one non-provisional tag, labelled by `layer.label` (the
/// key's last segment when unlabelled); channel and region values are the `dimension_value` vocabularies. Profiles are the published `profile` rows ([`db_profiles::profiles`]: newest non-provisional tag, drafts not offered, labelled by `label`, `industry` = the profile's industry, `note` = its description); areas are the `layer.areas` of the department heads (one choice per area, scoped to its department). An empty platform yields empty lists, never an error.
pub async fn db_options(c: &mut PgConnection) -> ApiResult<SelectionOptions> {
    let heads: Vec<LayerHead> =
        sqlx::query_as(
            "SELECT l.key, l.scope, l.industry, l.department, l.label, l.extends, l.areas FROM layer l \
              WHERE l.family = l.key AND l.scope IN ('industry', 'industry_dept') \
                AND EXISTS (SELECT 1 FROM layer_tag t WHERE t.family = l.key AND NOT t.provisional) \
                AND l.retired_at IS NULL \
                AND NOT EXISTS (SELECT 1 FROM layer i WHERE i.key = 'industry/' || l.industry \
                                                       AND i.retired_at IS NOT NULL) \
              ORDER BY l.key",
        )
        .fetch_all(&mut *c)
        .await?;
    let mut industries = Vec::new();
    let mut departments = Vec::new();
    let mut areas: Vec<SelectionChoice> = Vec::new();
    for (key, scope, industry, department, label, extends, dept_areas) in heads {
        let Some(industry) = industry else { continue };
        if scope == "industry" {
            industries.push(SelectionChoice {
                label: label.unwrap_or_else(|| industry.clone()),
                key: industry,
                note: None,
                extends: None,
                industry: None,
                industry_dept: None,
            });
        } else {
            let Some(department) = department else {
                continue;
            };
            for area in dept_areas.unwrap_or_default() {
                areas.push(SelectionChoice {
                    label: area.clone(),
                    key: area,
                    note: None,
                    extends: None,
                    industry: Some(industry.clone()),
                    industry_dept: Some(key.clone()),
                });
            }
            departments.push(SelectionChoice {
                label: label.unwrap_or(department),
                key,
                note: None,
                extends,
                industry: Some(industry),
                industry_dept: None,
            });
        }
    }
    areas.sort_by(|a, b| (&a.industry_dept, &a.key).cmp(&(&b.industry_dept, &b.key)));
    areas.dedup_by(|a, b| a.industry_dept == b.industry_dept && a.key == b.key);
    let values = |rows: Vec<vocab::DimensionValueRow>| -> Vec<SelectionChoice> {
        rows.into_iter()
            .map(|v| SelectionChoice {
                key: v.key,
                label: v.label,
                note: v.note,
                extends: None,
                industry: None,
                industry_dept: None,
            })
            .collect()
    };
    let channel = values(vocab::dimension_values(&mut *c, Some("channel")).await?);
    let region = values(vocab::dimension_values(&mut *c, Some("region")).await?);
    let profiles = db_profiles::profiles(&mut *c)
        .await?
        .into_iter()
        .map(|p| SelectionChoice {
            key: p.key,
            label: p.fields.label,
            note: Some(p.fields.description).filter(|d| !d.is_empty()),
            extends: None,
            industry: p.fields.industry,
            industry_dept: None,
        })
        .collect();
    Ok(SelectionOptions {
        profiles,
        industries,
        departments,
        areas,
        dimensions: SelectionOptionDimensions {
            channel: SelectionDimension {
                key: "channel".into(),
                label: "Channel".into(),
                values: channel,
                kind: Some("dimension".into()),
                note: None,
            },
            region: SelectionDimension {
                key: "region".into(),
                label: "Region".into(),
                values: region,
                kind: Some("placeholder".into()),
                note: None,
            },
        },
    })
}

// ---- LayerCompiler -----------------------------------------------------------------------------------------------

#[derive(Default)]
struct CacheInner {
    map: HashMap<String, Compiled>,
    /// Least recently used first.
    order: VecDeque<String>,
}

impl CacheInner {
    fn touch(&mut self, key: &str) {
        self.order.retain(|k| k != key);
        self.order.push_back(key.to_owned());
    }
}

/// The in-process bundle LRU. Never held across an `.await`.
#[derive(Default)]
struct BundleCache {
    inner: Mutex<CacheInner>,
}

impl BundleCache {
    fn get(&self, key: &str) -> Option<Compiled> {
        let mut g = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let hit = g.map.get(key).cloned();
        if hit.is_some() {
            g.touch(key);
        }
        hit
    }

    fn put(&self, key: String, compiled: Compiled) {
        let mut g = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        g.touch(&key);
        g.map.insert(key, compiled);
        while g.order.len() > BUNDLE_CACHE_CAP {
            if let Some(old) = g.order.pop_front() {
                g.map.remove(&old);
            }
        }
    }

    fn entries(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .map
            .len()
    }
}

/// The production compiler: B6a's pinned compile over the platform's rows (see the module docs for the mapping).
pub struct LayerCompiler {
    cache: BundleCache,
}

impl Default for LayerCompiler {
    fn default() -> Self {
        Self::new()
    }
}

impl LayerCompiler {
    /// No content of its own: only the in-process bundle cache.
    pub fn new() -> Self {
        Self {
            cache: BundleCache::default(),
        }
    }

    /// Bundles in the cache.
    pub fn cached(&self) -> usize {
        self.cache.entries()
    }

    async fn compile_uncached(
        &self,
        c: &mut PgConnection,
        sel: &Selection,
        pins: &[TemplatePin],
    ) -> Result<Compiled, CompileError> {
        let b6 = match templates::compile_pinned(c, pins, sel, job_key(pins)).await? {
            Ok(compiled) => compiled,
            Err(errors) => return Err(b6_failure(&errors)),
        };
        let Some(profile_key) = sel.profile.as_deref() else {
            return Err(failed("selection.profile", "A release needs a profile."));
        };
        let profile_values = db_profiles::pinned_profile(&mut *c, profile_key, pins)
            .await?
            .map(|p| p.values())
            .ok_or_else(|| failed("selection.profile", "The pinned profile is missing."))?;
        let mut bundle = Bundle {
            format: BUNDLE_FORMAT.to_owned(),
            sections: Vec::new(),
            objects: BundleObjects::default(),
            settings: BundleSettings::default(),
            rules: rule_slot_items(&b6.catalogue.rule_slots)?,
            requires: Requires::default(),
            causes: BTreeMap::new(),
            profile_values,
        };
        bundle.objects.activity = activity_items(&b6.knowledge.activities);
        fill_derived_ops(
            &mut bundle.objects.activity,
            &b6.effective.job_content.context_spaces,
            &b6.effective.job_content.procedures,
        );
        bundle.objects.context_space =
            items_of(&b6.effective.job_content.context_spaces, context_space_item);
        bundle.objects.knowledge_store = items_of(
            &b6.effective.job_content.knowledge_stores,
            knowledge_store_item,
        );
        bundle.objects.ontology_attribute = items_of(&b6.effective.attributes, attribute_item);
        bundle.objects.gate_option = gate_options(&b6.catalogue.gates);
        bundle.settings = settings_of(&b6);
        bundle.objects.instruction = instruction_items(&b6.catalogue.instructions);
        sync_sections(&mut bundle, &b6.catalogue.gates, sel);
        bundle.objects.knowledge_pack = knowledge_pack_items(&mut *c, pins, sel).await?;
        let derived = derive::derive(pins, sel, &bundle.objects);
        bundle.requires = derived.requires;
        bundle.causes = derived.causes;
        finish(bundle, derived.violations)
    }
}

/// The overlay base of PK-2's `knowledge_pack`: one item per (stage, kind, pack key) of the kinds the template seeds
/// (`kinds::template_seeds` without a job, so no tombstone applies), key `<stage>|<kind>|<pack_key>`, `label`
/// `<pack_key>@v<version> → <stage>/<kind>`, `pin` `<pack_key>@<version>`, and `replaces` `<stage>|<kind>|<old key>`
/// for a pack renamed by an upgrade (D112: the job's tombstone of the old key follows it, `release::conflicts`); in
/// key order (bytewise). Reads layer data only, so the compile cache key (row, layer and tag counts) covers it.
async fn knowledge_pack_items(
    c: &mut PgConnection,
    pins: &[TemplatePin],
    sel: &Selection,
) -> ApiResult<Vec<OverlayItem>> {
    let seeds = kinds::template_seeds(&mut *c, pins, Some(sel), None).await?;
    if seeds.iter().all(|s| s.packs.is_empty()) {
        return Ok(Vec::new());
    }
    // D112: pack key → the pack key it replaces (a renamed pack)
    let replaces: BTreeMap<String, String> = template_packs::fold(&mut *c, pins, sel)
        .await?
        .into_iter()
        .filter_map(|r| Some((r.pack_key, r.replaces?)))
        .collect();
    let mut items: BTreeMap<String, OverlayItem> = BTreeMap::new();
    for s in &seeds {
        for pin in s.packs.iter().filter_map(|p| kinds::parse_pin(p)) {
            let key = format!("{}|{}|{}", s.stage, s.kind, pin.key);
            let mut m = Map::new();
            m.insert("key".to_owned(), Value::String(key.clone()));
            m.insert(
                "label".to_owned(),
                Value::String(format!(
                    "{}@v{} → {}/{}",
                    pin.key, pin.version, s.stage, s.kind
                )),
            );
            m.insert("pin".to_owned(), Value::String(pin.pin()));
            // absent unless renamed, so a bundle without renamed packs hashes as before (PK-3)
            if let Some(old) = replaces.get(&pin.key) {
                m.insert(
                    "replaces".to_owned(),
                    Value::String(format!("{}|{}|{old}", s.stage, s.kind)),
                );
            }
            items.insert(key, m);
        }
    }
    Ok(items.into_values().collect())
}

/// The store's revision for the cache key: row, layer and tag counts (rows are append-only, tags immutable).
async fn store_revision(c: &mut PgConnection) -> ApiResult<String> {
    let (rows, layers, tags): (i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM layer_row), (SELECT count(*) FROM layer), \
                (SELECT count(*) FROM layer_tag)",
    )
    .fetch_one(c)
    .await?;
    Ok(format!("{COMPILER_REVISION}/{rows}.{layers}.{tags}"))
}

fn cache_key(sel: &Selection, pins: &[TemplatePin], revision: &str) -> ApiResult<String> {
    let v = json!({"selection": sel, "pins": pins, "revision": revision});
    hash::bundle_hash(&v).map_err(|e| ApiError::Internal(anyhow::anyhow!("bundle cache key: {e}")))
}

/// B6a's compile errors: a refusal keeps its status and code, anything else is 422 `compile_failed` with one
/// `{path: "<code>:<key>", message}` per error.
fn b6_failure(errors: &[model::CompileError]) -> CompileError {
    if model::refusal(errors).is_some() {
        return CompileError::Api(Box::new(templates::compile_refusal(errors)));
    }
    CompileError::Failed(
        errors
            .iter()
            .map(|e| BundleViolation {
                path: if e.key.is_empty() {
                    e.code.clone()
                } else {
                    format!("{}:{}", e.code, e.key)
                },
                message: e.message.clone(),
            })
            .collect(),
    )
}

/// Compiled items (key → fields) as overlay items in key order, each shaped by `shape`.
fn items_of(
    items: &model::Items,
    shape: fn(&str, &Map<String, Value>) -> OverlayItem,
) -> Vec<OverlayItem> {
    items
        .iter()
        .map(|(key, fields)| shape(key, fields))
        .collect()
}

/// `{key, ..fields}` with B6a field names renamed (`renames`: from → to) and missing fields defaulted.
fn renamed(
    key: &str,
    fields: &Map<String, Value>,
    renames: &[(&str, &str)],
    defaults: &[(&str, Value)],
) -> OverlayItem {
    let mut m = Map::new();
    m.insert("key".to_owned(), Value::String(key.to_owned()));
    for (k, v) in fields {
        if k == "key" {
            continue;
        }
        let name = renames
            .iter()
            .find(|(from, _)| *from == k.as_str())
            .map_or(k.as_str(), |(_, to)| *to);
        m.insert(name.to_owned(), v.clone());
    }
    for (k, v) in defaults {
        m.entry(*k).or_insert_with(|| v.clone());
    }
    m
}

/// A context space in the fixture shape: `slots` → `required`; `required`/`entities` default `[]`, `label` the key.
fn context_space_item(key: &str, fields: &Map<String, Value>) -> OverlayItem {
    renamed(
        key,
        fields,
        &[("slots", "required")],
        &[
            ("label", Value::String(key.to_owned())),
            ("required", Value::Array(Vec::new())),
            ("entities", Value::Array(Vec::new())),
        ],
    )
}

/// An ontology attribute in the fixture shape: `data_type` → `type`; `label` defaults to the key.
fn attribute_item(key: &str, fields: &Map<String, Value>) -> OverlayItem {
    renamed(
        key,
        fields,
        &[("data_type", "type")],
        &[("label", Value::String(key.to_owned()))],
    )
}

/// A knowledge store in the fixture shape: `label` defaults to the key; `kind` = the family of the first of `kinds`
/// (before the first `.`; omitted when there is none); `kinds` is kept.
fn knowledge_store_item(key: &str, fields: &Map<String, Value>) -> OverlayItem {
    let mut m = renamed(
        key,
        fields,
        &[],
        &[("label", Value::String(key.to_owned()))],
    );
    if let Some(first) = fields
        .get("kinds")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(Value::as_str)
    {
        let family = first.split('.').next().unwrap_or(first);
        m.entry("kind")
            .or_insert_with(|| Value::String(family.to_owned()));
    }
    m
}

/// The generated options of the compiled gates, in gate order then option order, in the fixture shape
/// `{key: "<gate> · <option>", label, generated}` (+ `description` when non-empty).
fn gate_options(gates: &[model::CatalogueItem]) -> Vec<OverlayItem> {
    let mut out = Vec::new();
    for g in gates {
        let Some(Value::Array(options)) = g.fields.get("options") else {
            continue;
        };
        for o in options {
            let Some(opt) = o.as_object() else {
                continue;
            };
            if opt.get("generated") != Some(&Value::Bool(true)) {
                continue;
            }
            let Some(key) = opt.get("key").and_then(Value::as_str) else {
                continue;
            };
            let label = opt.get("label").and_then(Value::as_str).unwrap_or(key);
            let mut m = Map::new();
            m.insert(
                "key".to_owned(),
                Value::String(format!("{} · {key}", g.key)),
            );
            m.insert("label".to_owned(), Value::String(label.to_owned()));
            m.insert("generated".to_owned(), Value::Bool(true));
            if let Some(d) = opt.get("description").and_then(Value::as_str)
                && !d.is_empty()
            {
                m.insert("description".to_owned(), Value::String(d.to_owned()));
            }
            out.push(m);
        }
    }
    out
}

/// The fixture's order of the job settings in the `settings` section.
const SETTINGS_SECTION_ORDER: [&str; 3] = ["actor", "stance", "preamble"];

/// Replace section `key`'s items; append the section when it is absent and there is content.
fn set_section(bundle: &mut Bundle, key: &str, label: &str, items: Vec<String>) {
    if let Some(i) = bundle.sections.iter().position(|s| s.key == key) {
        bundle.sections[i].items = items;
    } else if !items.is_empty() {
        bundle.sections.push(BundleSection {
            key: key.to_owned(),
            label: label.to_owned(),
            items,
        });
    }
}

fn strings_of(item: &OverlayItem, field: &str) -> Vec<String> {
    item.get(field)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// The fixture-shaped `objects.activity` items of the KH fold (R2): `{key, stage, label, actor, required, ops?}`, in
/// key order. `required` is the winning row's flag (owner 2026-10-26); `ops` (owner 2026-10-26, D62 backfill) are the
/// operations the row names, in the shape `job_design::base::ActivityOp` reads, so a release carries its own grants
/// and needs no later fold; absent when the row names none ([`fill_derived_ops`] adds the derived ones to a cut).
/// An archetype's activity (stage `*`, D107) has no stage column and is not an overlay base.
pub fn activity_items(activities: &[model::FoldedActivity]) -> Vec<OverlayItem> {
    activities
        .iter()
        .filter(|a| a.stage != agent_model::kinds::stage_knowledge::ANY_STAGE)
        .map(|a| {
            let actor = match a.actor {
                Some(agent_model::kinds::journey::ActivityActor::Company) => "company",
                Some(agent_model::kinds::journey::ActivityActor::Employee) => "employee",
                _ => "customer",
            };
            let mut m = Map::new();
            m.insert("key".into(), Value::String(a.key.clone()));
            m.insert("stage".into(), Value::String(a.stage.clone()));
            m.insert("label".into(), Value::String(a.label.clone()));
            m.insert("actor".into(), Value::String(actor.to_owned()));
            m.insert("required".into(), Value::Bool(a.required));
            if !a.operations.is_empty() {
                let ops = a.operations.iter().map(|o| ActivityOp {
                    op: o.op.clone(),
                    variant: o.variant.clone(),
                    required: o.required,
                });
                m.insert("ops".into(), ops_value(ops));
            }
            m
        })
        .collect()
}

/// `ops` as a bundle item stores them: `[{op, variant, required}]` ([`ActivityOp`]'s serde shape).
fn ops_value(ops: impl IntoIterator<Item = ActivityOp>) -> Value {
    Value::Array(
        ops.into_iter()
            .filter_map(|o| serde_json::to_value(o).ok())
            .collect(),
    )
}

/// A cut is self-contained: an activity whose row names no operations takes those of the procedure of the context
/// space that serves it (`job_design::base::derived_ops`), written into its bundle item as `ops`.
pub fn fill_derived_ops(items: &mut [OverlayItem], spaces: &Items, procedures: &Items) {
    for item in items {
        if item.contains_key("ops") {
            continue;
        }
        let Some(key) = item.get("key").and_then(Value::as_str) else {
            continue;
        };
        let derived = derived_ops(key, spaces, procedures);
        if !derived.is_empty() {
            item.insert("ops".into(), ops_value(derived));
        }
    }
}

/// Rebuild the sections that describe B6a-sourced parts from those parts (see the module docs).
fn sync_sections(bundle: &mut Bundle, gates: &[model::CatalogueItem], sel: &Selection) {
    let activities: Vec<String> = bundle
        .objects
        .activity
        .iter()
        .filter_map(|i| {
            let key = i.get("key")?.as_str()?;
            let label = i.get("label").and_then(Value::as_str).unwrap_or(key);
            Some(format!("{key} — {label}"))
        })
        .collect();
    let spaces: Vec<String> = bundle
        .objects
        .context_space
        .iter()
        .filter_map(|i| i.get("key").and_then(Value::as_str))
        .map(str::to_owned)
        .collect();
    let mut kinds: Vec<String> = Vec::new();
    let from_stores = bundle
        .objects
        .knowledge_store
        .iter()
        .flat_map(|i| strings_of(i, "kinds"));
    let from_spaces = bundle
        .objects
        .context_space
        .iter()
        .flat_map(|i| strings_of(i, "knowledge_kinds"));
    for k in from_stores.chain(from_spaces) {
        if !kinds.contains(&k) {
            kinds.push(k);
        }
    }
    let gate_keys: Vec<String> = gates
        .iter()
        .map(|g| {
            if g.fields.get("fail").and_then(Value::as_str) == Some("closed") {
                format!("{} (closed)", g.key)
            } else {
                g.key.clone()
            }
        })
        .collect();
    let settings: Vec<String> = SETTINGS_SECTION_ORDER
        .iter()
        .filter(|k| bundle.settings.get(k).is_some())
        .map(|k| (*k).to_owned())
        .collect();
    set_section(bundle, "context_spaces", "Context spaces", spaces);
    set_section(bundle, "activities", "Activities", activities);
    bundle.sections.push(BundleSection {
        key: "channels".to_owned(),
        label: "Channels".to_owned(),
        items: sel.dimensions.channel.clone(),
    });
    bundle.sections.push(BundleSection {
        key: "regions".to_owned(),
        label: "Regions".to_owned(),
        items: sel.values("region").to_vec(),
    });
    set_section(bundle, "gates", "Gate questions", gate_keys);
    set_section(bundle, "knowledge_kinds", "Knowledge kinds", kinds);
    set_section(bundle, "settings", "Job settings", settings);
}

/// The settings bases of a B6a compile (D108): `preamble` is the job's folded `identity` instruction (a `job` row's
/// `preamble` and a `job_setting` keyed `preamble` fold as their layer's `identity`, see
/// `agent_model::kinds::instruction`); `actor` and `stance` are the string `value` of the folded `job_setting` of
/// that key — the compile folds a `job` row's `actor` / `stance` as its layer's setting, so the nearest layer in fold
/// order wins whichever row kind set it. The runtime's Draft design reads the same bases (`runtime::design`).
pub fn settings_of(c: &model::Compiled) -> BundleSettings {
    let setting = |k: &str| {
        c.catalogue
            .job_settings
            .iter()
            .find(|s| s.key == k)
            .and_then(|s| s.fields.get("value"))
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    BundleSettings {
        preamble: slot_text(&c.catalogue.instructions, IDENTITY).map(str::to_owned),
        actor: setting("actor"),
        stance: setting("stance"),
    }
}

impl TemplateCompiler for LayerCompiler {
    fn compile<'a>(
        &'a self,
        c: &'a mut PgConnection,
        sel: &'a Selection,
        pins: &'a [TemplatePin],
    ) -> BoxFuture<'a, Result<Compiled, CompileError>> {
        Box::pin(async move {
            let revision = store_revision(&mut *c).await?;
            let key = cache_key(sel, pins, &revision)?;
            if let Some(hit) = self.cache.get(&key) {
                return Ok(hit);
            }
            let compiled = self.compile_uncached(c, sel, pins).await?;
            self.cache.put(key, compiled.clone());
            Ok(compiled)
        })
    }

    /// D128: the options come from the platform's DB, never compiled in ([`db_options`]).
    fn options<'a>(
        &'a self,
        c: &'a mut PgConnection,
    ) -> BoxFuture<'a, ApiResult<SelectionOptions>> {
        Box::pin(async move { db_options(c).await })
    }

    fn latest_tags<'a>(
        &'a self,
        c: &'a mut PgConnection,
    ) -> BoxFuture<'a, ApiResult<BTreeMap<String, i32>>> {
        Box::pin(async move { templates::latest_tags(c).await })
    }

    fn stack<'a>(
        &'a self,
        c: &'a mut PgConnection,
        pin: &'a TemplatePin,
    ) -> BoxFuture<'a, ApiResult<Vec<TemplatePin>>> {
        Box::pin(async move {
            let tag = db_tags::tag(c, &pin.layer, pin.version).await?;
            serde_json::from_value(tag.stack).map_err(|e| {
                ApiError::Internal(anyhow::anyhow!(
                    "layer_tag {}@v{} has an unreadable stack: {e}",
                    pin.layer,
                    pin.version
                ))
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pin(layer: &str, version: i32) -> TemplatePin {
        TemplatePin {
            layer: layer.to_owned(),
            version,
        }
    }

    #[test]
    fn job_pin_helpers() {
        let pins = [pin("base", 3), pin("job/customer_care_agent", 4)];
        assert_eq!(job_template_version(&pins), 4);
        assert_eq!(job_key(&pins), Some("customer_care_agent"));
        assert_eq!(job_template_version(&pins[..1]), 0);
        assert_eq!(job_key(&pins[..1]), None);
    }

    /// A slot the `rule_slot` kind accepts (its defaulted `inputs`, `values` and value `hint` left out) reads as a
    /// bundle rule: a stored row never fails the compile on a field the authoring contract made optional.
    #[test]
    fn a_rule_slot_with_only_its_required_fields_reads() {
        let fields = json!({"label": "Pending payment", "applies_to": ["payment.refund"], "outcome": "eligible | ineligible",
            "values": [{"key": "drop_off_days", "label": "drop off days", "unit": "days"}]});
        agent_model::kinds::validate_fields(agent_model::kinds::Kind::RuleSlot, &fields)
            .expect("the kind accepts it");
        let item = |fields: Value| model::CatalogueItem {
            key: "PAY-04".into(),
            row_id: "r".into(),
            layer: "industry/retail".into(),
            fields: fields.as_object().unwrap().clone(),
        };
        let slots = rule_slot_items(&[item(fields)]).expect("reads");
        assert_eq!(slots[0].values[0].hint, "");
        assert!(slots[0].inputs.is_empty());
        let bare = rule_slot_items(&[item(
            json!({"label": "x", "applies_to": [], "outcome": "o"}),
        )])
        .expect("reads");
        assert!(bare[0].values.is_empty());
    }

    #[test]
    fn compile_errors_map_to_their_responses() {
        let v = vec![BundleViolation {
            path: "unpinned_layer:industry/retail".into(),
            message: "m".into(),
        }];
        let e: ApiError = CompileError::Failed(v.clone()).into();
        assert_eq!(
            e.status_and_code(),
            (StatusCode::UNPROCESSABLE_ENTITY, "compile_failed")
        );
        assert_eq!(
            bundle_incomplete(&v).status_and_code(),
            (StatusCode::UNPROCESSABLE_ENTITY, "bundle_incomplete")
        );
        let refused = b6_failure(&[model::CompileError {
            code: model::code::PROFILE_NOT_FOUND.to_owned(),
            key: "nope".into(),
            row_id: None,
            message: "No profile nope.".into(),
            layers: Vec::new(),
        }]);
        let e: ApiError = refused.into();
        assert_eq!(
            e.status_and_code(),
            (StatusCode::NOT_FOUND, "profile_not_found")
        );
    }
}
