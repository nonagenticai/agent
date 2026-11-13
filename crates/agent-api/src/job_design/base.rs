//! The job's base objects at its design's pins (plan B5 T4, R1/R3): READ, never copied into B5 tables. Every B5 row
//! refines or extends an object named here, by its natural key (+ the base's hash for conflict detection).
//!
//! | Design | Pins, selection | Activities, generated gate options | Languages (gate B6) |
//! |---|---|---|---|
//! | `Release(id)` | the release's `pins` / `selection` | its stored `bundle.objects` | its snapshot's `languages` place |
//! | `Live`, released | as `Release(jobs.current_release_id)` | | |
//! | `Live`, never released | `jobs.pins` + the BASELINE selection | the bundle that selection compiles to now | the baseline's |
//! | `Draft` | `jobs.pins` + the DRAFT selection (what B4 compiles) | the bundle the draft compiles to | the working row |
//!
//! - Entities, attributes, relations, context spaces, procedures (`name@N`, step counts only), the gate library
//!   (`catalogue.gates`, every option incl. the generated ones), the agent roles and the operations come from B6a's
//!   [`compile_pinned`](crate::platform::templates::compile_pinned) at those pins. Tagged layers are immutable, so a
//!   compile of a release's pins is reproducible; it is memoised in-process (key: pins, selection, job, and the
//!   platform store's row / layer / tag counts — as `release::compile`'s bundle cache — so any store change that
//!   can alter a compile misses).
//! - Activities and the generated gate options are B2's bundle `objects.activity` / `objects.gate_option` (B2
//!   overlays them; B5 never stores them). R2 (B8 phase 2): the KH fold of the pinned `activity` rows
//!   (`Compiled.knowledge.activities`, `FoldedActivity.operations`) is the ONE source of an activity's operations
//!   (seed v8 carries them; the old `seed/job_design/activity_ops.json` is gone) and, for a job that was never
//!   released or a draft, of the activity list itself (`LayerCompiler` builds `objects.activity` from it). An
//!   activity whose folded row names no operations takes them from the procedure of the base context space that
//!   serves it ([`derived_ops`]); one with neither has none (logged once per process as `activity_ops_missing`).
//!   A released bundle keeps its own activity list (D62, immutable) and, since the owner decision of 2026-10-26, its own
//!   operations: each item's `ops` (written by every cut; migration 0194 backfilled the releases cut before v8 from the
//!   old file) win over the fold at its pins, which only serves an item without them. The bundle's context_space items
//!   likewise carry `activity`, which wins over the compile at the pins in [`Bases::space_activity`].
//! - R15: a base context space names the activity it serves in its item's `activity` (`Bases::space_activity`; seed
//!   v8 sets it on the retail spaces). A space without one has no activity: its procedures' grant is empty.
//!   [`context_space_for`] is the reverse lookup B8 uses (else the D61 key).
//! - D59: gate B6's options are the languages the job serves plus `other` (web `forJob`): an option the library row
//!   already has keeps its fields, a new one gets the language's label. No languages → the library row as-is.
//! - A template that does not compile (compile errors, an untagged pin, a refused profile) gives EMPTY bases and its
//!   errors in [`Bases::template_errors`] (`code:key`, as B4's `RuntimeDesign.template_errors`), never a 500.
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};

use agent_model::catalog::OperationRow;
use agent_model::compile::{CatalogueItem, Compiled, Items, LayerMeta, Selection};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sqlx::PgConnection;

use crate::design::hooks::is_template_refusal;
use crate::design::read::values_of_kind;
use crate::design::{DesignOf, JobRef, Registry, snap_from_json};
use crate::error::{ApiError, ApiResult};
use crate::platform::templates::{self, TemplatePin};
use crate::release::bundle::OverlayItem;
use crate::release::compile::{CompileError, TemplateCompiler, job_key};
use crate::release::hash::{bundle_hash, decimals_as_text, row_hash};
use crate::routes::release::settings::LANGUAGES;

/// The gate whose options are the job's languages (D59).
pub const LANGUAGE_GATE: &str = "B6";
/// Compiles kept by the in-process memo.
pub const COMPILE_CACHE_CAP: usize = 64;

/// One operation an activity expects (fixture `ActivityRow.ops`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActivityOp {
    /// `object.verb`.
    pub op: String,
    pub variant: Option<String>,
    pub required: bool,
}

/// `(layer, activity)` pairs already reported as `activity_ops_missing` (logged once per process).
static MISSING_LOGGED: LazyLock<Mutex<BTreeSet<(String, String)>>> =
    LazyLock::new(|| Mutex::new(BTreeSet::new()));

/// The job-template layer of a pin set: the first `job/*` pin (`job/customer_care_agent`).
pub fn job_layer(pins: &[TemplatePin]) -> Option<&str> {
    pins.iter()
        .find(|p| p.layer.starts_with("job/"))
        .map(|p| p.layer.as_str())
}

/// One base activity: B2's bundle item (`{key, stage, label, actor, required}`) and the seed's operations.
#[derive(Clone, Debug, PartialEq)]
pub struct BaseActivity {
    pub key: String,
    pub item: OverlayItem,
    pub ops: Vec<ActivityOp>,
}

/// The job's base objects at one design's pins (module docs).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Bases {
    pub pins: Vec<TemplatePin>,
    pub selection: Selection,
    /// The release the bases were read at (`Release`, or `Live` of a released job); `None` = compiled from the job.
    pub release_id: Option<String>,
    /// The languages the design serves (gate B6's options).
    pub languages: Vec<String>,
    /// B6a's `effective_hash` of the compile (empty when the template did not compile).
    pub effective_hash: String,
    pub entities: Items,
    /// Keyed `Entity.attr`.
    pub attributes: Items,
    /// Keyed `Subject|predicate|Object`.
    pub relations: Items,
    /// The layer that first wrote each entity and attribute (B6a provenance, keyed `entity:<name>` /
    /// `attribute:<Entity.attr>`): J11 shows it as the row's `layer` and derives its `scope` from it.
    pub authored_in: BTreeMap<String, LayerMeta>,
    /// Keyed by the D61 key.
    pub context_spaces: Items,
    /// Keyed `name@N` (no step bodies).
    pub procedures: Items,
    /// The gate library at the pins, B6's options already the job's languages.
    pub gates: Vec<CatalogueItem>,
    pub agent_roles: Vec<CatalogueItem>,
    pub operations: Vec<OperationRow>,
    /// The job's activities with their operations (module docs, R2).
    pub activities: Vec<BaseActivity>,
    /// R2: the KH fold's operations by activity key (kept so a released bundle's own activity list takes them too).
    pub(crate) fold_ops: BTreeMap<String, Vec<ActivityOp>>,
    /// B2's `objects.gate_option`: the GENERATED options only, keyed `<GATE> · <option>`.
    pub gate_options: Vec<OverlayItem>,
    /// Why the template did not compile (`code:key`); empty when it did.
    pub template_errors: Vec<String>,
}

impl Bases {
    pub fn entity(&self, name: &str) -> Option<&Map<String, Value>> {
        self.entities.get(name)
    }

    pub fn gate(&self, key: &str) -> Option<&CatalogueItem> {
        self.gates.iter().find(|g| g.key == key)
    }

    /// Option `option` of gate `gate` as the library row has it (generated options included).
    pub fn gate_option(&self, gate: &str, option: &str) -> Option<&Map<String, Value>> {
        gate_options_of(self.gate(gate)?)
            .find(|o| o.get("key").and_then(Value::as_str) == Some(option))
    }

    pub fn role(&self, key: &str) -> Option<&CatalogueItem> {
        self.agent_roles.iter().find(|r| r.key == key)
    }

    /// The activity base context space `space` serves: its item's `activity` (R15); `None` when it names none.
    pub fn space_activity(&self, space: &str) -> Option<String> {
        let own = self
            .context_spaces
            .get(space)
            .and_then(|f| f.get("activity"))
            .and_then(Value::as_str)
            .filter(|a| !a.is_empty());
        own.map(str::to_owned)
    }

    pub fn activity(&self, key: &str) -> Option<&BaseActivity> {
        self.activities.iter().find(|a| a.key == key)
    }

    /// The base versions of procedure `name`, ascending.
    pub fn procedure_versions(&self, name: &str) -> Vec<u32> {
        let mut out: Vec<u32> = self
            .procedures
            .keys()
            .filter_map(|k| {
                let (n, v) = k.rsplit_once('@').unwrap_or((k.as_str(), "1"));
                (n == name).then(|| v.parse().unwrap_or(1))
            })
            .collect();
        out.sort_unstable();
        out
    }
}

/// B8: the base context space that serves `activity` at the job's pins ([`Bases::space_activity`]), the first by
/// key; `None` when no base space does (B8 then makes the D61 key from the activity's area).
pub fn context_space_for(bases: &Bases, activity: &str) -> Option<String> {
    bases
        .context_spaces
        .keys()
        .find(|k| bases.space_activity(k).as_deref() == Some(activity))
        .cloned()
}

/// The option objects of a library gate (`fields.options`).
pub fn gate_options_of(gate: &CatalogueItem) -> impl Iterator<Item = &Map<String, Value>> {
    gate.fields
        .get("options")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_object)
}

fn internal(what: String) -> ApiError {
    ApiError::Internal(anyhow::anyhow!(what))
}

/// Every base hash: decimals (a gate's `threshold_value`, a rule's `min_confidence`, at any depth) hash as their
/// JSON text ([`decimals_as_text`]), since `row_hash` is integers-only (RFC 8785 subset).
fn hash_of(v: &Value, strip: &[&str], what: &str) -> ApiResult<String> {
    row_hash(&decimals_as_text(v.clone()), strip)
        .map_err(|e| internal(format!("{what} does not hash: {e}")))
}

/// `base_hash` of a base entity (`ontology_entity_refinement`): its item without the `attributes` child array (the
/// attributes are B2 `ontology_attribute` places).
pub fn entity_hash(item: &Map<String, Value>) -> ApiResult<String> {
    hash_of(
        &Value::Object(item.clone()),
        &["attributes"],
        "a base entity",
    )
}

/// `base_hash` of a library gate option (`job_gate_option_refinement`).
pub fn gate_option_hash(option: &Map<String, Value>) -> ApiResult<String> {
    hash_of(&Value::Object(option.clone()), &[], "a gate option")
}

/// `base_hash` of a library gate's tunable rule (`job_gate_rule_refinement`): `{threshold_value, rule}`.
pub fn gate_rule_hash(gate: &CatalogueItem) -> ApiResult<String> {
    let f = &gate.fields;
    let v = json!({
        "threshold_value": f.get("threshold_value").cloned().unwrap_or(Value::Null),
        "rule": f.get("rule").cloned().unwrap_or(Value::Null),
    });
    hash_of(&v, &[], "a gate rule")
}

/// D59 (web `forJob`): gate B6's options = `languages` + `other`, each keeping the library option of that key, else
/// `{key, label: <language label | key>, description: "", generated: true}`. No languages → unchanged.
pub fn apply_languages(gates: &mut [CatalogueItem], languages: &[String]) {
    if languages.is_empty() {
        return;
    }
    let Some(g) = gates.iter_mut().find(|g| g.key == LANGUAGE_GATE) else {
        return;
    };
    let by_key: BTreeMap<String, Map<String, Value>> = gate_options_of(g)
        .filter_map(|o| Some((o.get("key")?.as_str()?.to_owned(), o.clone())))
        .collect();
    let options: Vec<Value> = languages
        .iter()
        .map(String::as_str)
        .chain(std::iter::once("other"))
        .map(|k| {
            Value::Object(by_key.get(k).cloned().unwrap_or_else(|| {
                let label = LANGUAGES
                    .iter()
                    .find(|(l, _)| *l == k)
                    .map_or(k, |(_, name)| *name);
                let mut m = Map::new();
                m.insert("key".into(), Value::String(k.to_owned()));
                m.insert("label".into(), Value::String(label.to_owned()));
                m.insert("description".into(), Value::String(String::new()));
                m.insert("generated".into(), Value::Bool(true));
                m
            }))
        })
        .collect();
    g.fields.insert("options".into(), Value::Array(options));
}

/// The items of `object` in a stored bundle (read leniently, as `release::conflicts`).
fn bundle_items(bundle: &Value, object: &str) -> Vec<OverlayItem> {
    bundle
        .get("objects")
        .and_then(|o| o.get(object))
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_object).cloned().collect())
        .unwrap_or_default()
}

/// The operations of `activity` read off the procedure of the base context space that serves it (KC: a department
/// layer's spaces name their activity, `fields.activity`): one entry per step that calls an operation, in step order,
/// once per (operation, variant); a read step is required, a write optional (the hand fixture's rule). Empty when no
/// space serves the activity or its procedure has no step bodies.
pub fn derived_ops(activity: &str, spaces: &Items, procedures: &Items) -> Vec<ActivityOp> {
    let steps = spaces
        .values()
        .filter(|f| f.get("activity").and_then(Value::as_str) == Some(activity))
        .filter_map(|f| f.get("procedure").and_then(Value::as_str))
        .filter_map(|name| procedures.get(name)?.get("steps")?.as_array())
        .next();
    let mut ops: Vec<ActivityOp> = Vec::new();
    for step in steps.into_iter().flatten().filter_map(Value::as_object) {
        let Some(op) = step.get("operation").and_then(Value::as_str) else {
            continue;
        };
        let variant = step
            .get("variant")
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
            .map(str::to_owned);
        if ops.iter().any(|o| o.op == op && o.variant == variant) {
            continue;
        }
        ops.push(ActivityOp {
            op: op.to_owned(),
            variant,
            required: step.get("side_effect").and_then(Value::as_str) == Some("R"),
        });
    }
    ops
}

/// The operations of a folded activity (R2): the winning `activity` row's `operations`.
fn fold_op(o: &agent_model::kinds::journey::OperationRef) -> ActivityOp {
    ActivityOp {
        op: o.op.clone(),
        variant: o.variant.clone(),
        required: o.required,
    }
}

/// The fold's operations by activity key (only the activities whose row names some).
fn fold_ops(c: &Compiled) -> BTreeMap<String, Vec<ActivityOp>> {
    c.knowledge
        .activities
        .iter()
        .filter(|a| !a.operations.is_empty())
        .map(|a| (a.key.clone(), a.operations.iter().map(fold_op).collect()))
        .collect()
}

/// The operations a bundle activity item carries in its `ops` (owner 2026-10-26: a cut is self-contained; migration
/// 0194 backfilled the releases cut before v8); `None` when it carries none (absent, empty or unreadable).
fn item_ops(item: &OverlayItem) -> Option<Vec<ActivityOp>> {
    let ops: Vec<ActivityOp> = serde_json::from_value(item.get("ops")?.clone()).ok()?;
    (!ops.is_empty()).then_some(ops)
}

/// Bundle activity items → [`BaseActivity`]: the operations the item carries (`ops`), else those the fold gives the
/// activity, else those derived from the base context space that serves it ([`derived_ops`]).
fn activities_of(
    items: Vec<OverlayItem>,
    fold: &BTreeMap<String, Vec<ActivityOp>>,
    spaces: &Items,
    procedures: &Items,
) -> Vec<BaseActivity> {
    items
        .into_iter()
        .filter_map(|item| {
            let key = item.get("key")?.as_str()?.to_owned();
            let ops = match (item_ops(&item), fold.get(&key)) {
                (Some(ops), _) => ops,
                (None, Some(ops)) => ops.clone(),
                (None, None) => derived_ops(&key, spaces, procedures),
            };
            if ops.is_empty() {
                let pair = ("-".to_owned(), key.clone());
                let fresh = MISSING_LOGGED
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(pair);
                if fresh {
                    tracing::warn!(activity = %key, "activity_ops_missing");
                }
            }
            Some(BaseActivity { key, item, ops })
        })
        .collect()
}

// ---- the compile memo -------------------------------------------------------------------------------------------

#[derive(Default)]
struct Memo {
    map: HashMap<String, Arc<Compiled>>,
    /// Least recently used first.
    order: VecDeque<String>,
}

static COMPILES: LazyLock<Mutex<Memo>> = LazyLock::new(|| Mutex::new(Memo::default()));

fn memo_get(key: &str) -> Option<Arc<Compiled>> {
    let mut g = COMPILES.lock().unwrap_or_else(PoisonError::into_inner);
    let hit = g.map.get(key).cloned();
    if hit.is_some() {
        g.order.retain(|k| k != key);
        g.order.push_back(key.to_owned());
    }
    hit
}

fn memo_put(key: String, c: Arc<Compiled>) {
    let mut g = COMPILES.lock().unwrap_or_else(PoisonError::into_inner);
    g.order.retain(|k| *k != key);
    g.order.push_back(key.clone());
    g.map.insert(key, c);
    while g.order.len() > COMPILE_CACHE_CAP {
        if let Some(old) = g.order.pop_front() {
            g.map.remove(&old);
        }
    }
}

/// B6a's compile at `pins` / `sel`, memoised; `Err(errors)` = the template does not compile (`code:key`, never
/// memoised). Database errors propagate.
async fn compile(
    c: &mut PgConnection,
    pins: &[TemplatePin],
    sel: &Selection,
) -> ApiResult<Result<Arc<Compiled>, Vec<String>>> {
    let job = job_key(pins);
    let (rows, layers, tags): (i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM layer_row), (SELECT count(*) FROM layer), \
                (SELECT count(*) FROM layer_tag)",
    )
    .fetch_one(&mut *c)
    .await?;
    let key = bundle_hash(&json!({
        "pins": pins,
        "selection": sel,
        "job": job,
        "revision": format!("{rows}.{layers}.{tags}"),
    }))
    .map_err(|e| internal(format!("base compile key: {e}")))?;
    if let Some(hit) = memo_get(&key) {
        return Ok(Ok(hit));
    }
    match templates::compile_pinned(&mut *c, pins, sel, job).await {
        Ok(Ok(compiled)) => {
            let compiled = Arc::new(compiled);
            memo_put(key, compiled.clone());
            Ok(Ok(compiled))
        }
        Ok(Err(errors)) => Ok(Err(errors
            .iter()
            .map(|e| format!("{}:{}", e.code, e.key))
            .collect())),
        Err(e) if is_template_refusal(&e) => Ok(Err(vec![refusal_code(&e)])),
        Err(e) => Err(e),
    }
}

/// The machine code of a template refusal (`template_not_tagged`, `tag_not_found`, …).
fn refusal_code(e: &ApiError) -> String {
    match e {
        ApiError::Missing(code, _) | ApiError::Conflict(code, _) => (*code).to_owned(),
        _ => "template_refused".to_owned(),
    }
}

/// The parts of [`Bases`] the B6a compile gives (empty with `template_errors` when it does not compile).
async fn from_compile(
    c: &mut PgConnection,
    pins: Vec<TemplatePin>,
    selection: Selection,
    languages: Vec<String>,
) -> ApiResult<Bases> {
    let mut b = Bases {
        pins,
        selection,
        languages,
        ..Bases::default()
    };
    match compile(&mut *c, &b.pins, &b.selection).await? {
        Ok(compiled) => {
            let eff = &compiled.effective;
            b.effective_hash = eff.hash.clone();
            b.entities = eff.entities.clone();
            b.attributes = eff.attributes.clone();
            b.relations = eff.relations.clone();
            b.authored_in = eff
                .provenance
                .iter()
                .filter(|(k, _)| k.starts_with("entity:") || k.starts_with("attribute:"))
                .filter_map(|(k, origins)| Some((k.clone(), origins.first()?.layer.clone())))
                .collect();
            b.context_spaces = eff.job_content.context_spaces.clone();
            b.procedures = eff.job_content.procedures.clone();
            b.gates = compiled.catalogue.gates.clone();
            b.agent_roles = compiled.catalogue.agent_roles.clone();
            b.operations = compiled.catalogue.operations.clone();
            apply_languages(&mut b.gates, &b.languages);
            // R2: the folded activities (a job that was never released, a draft)
            b.fold_ops = fold_ops(&compiled);
            b.activities = activities_of(
                crate::release::compile::activity_items(&compiled.knowledge.activities),
                &b.fold_ops,
                &b.context_spaces,
                &b.procedures,
            );
        }
        Err(errors) => b.template_errors = errors,
    }
    Ok(b)
}

#[derive(sqlx::FromRow)]
struct ReleaseRow {
    pins: Value,
    selection: Option<Value>,
    bundle: Value,
}

/// The bases of release `release_id` of `job` (its pins, selection and stored bundle); `None` when the job has no
/// such release. Needs no registry or compiler: the conflict detector runs on this.
pub async fn of_release(
    c: &mut PgConnection,
    job: &JobRef,
    release_id: &str,
) -> ApiResult<Option<Arc<Bases>>> {
    let row = sqlx::query_as::<_, ReleaseRow>(
        "SELECT pins, selection, bundle FROM job_release WHERE job_id = $1 AND release_id = $2",
    )
    .bind(job.id)
    .bind(release_id)
    .fetch_optional(&mut *c)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let pins: Vec<TemplatePin> = serde_json::from_value(row.pins).map_err(|e| {
        internal(format!(
            "job_release.pins of {release_id} is unreadable: {e}"
        ))
    })?;
    // R15: a release cut before KH keeps the legacy shape (immutable, D62); upgraded in memory
    let selection: Selection = match row.selection {
        Some(v) => crate::design::read::selection_of_value(&v).map_err(|e| {
            internal(format!(
                "job_release.selection of {release_id} is unreadable: {e}"
            ))
        })?,
        None => Selection::default(),
    };
    let stored: Option<Option<Value>> = sqlx::query_scalar(
        "SELECT snap FROM release_snapshot WHERE job_id = $1 AND release_id = $2 AND key = 'languages'",
    )
    .bind(job.id)
    .bind(release_id)
    .fetch_optional(&mut *c)
    .await?;
    let languages = languages_of(snap_from_json(stored.flatten())?);
    let mut b = from_compile(&mut *c, pins, selection, languages).await?;
    b.release_id = Some(release_id.to_owned());
    // the cut's own space -> activity mapping wins over the compile at its pins (a pre-v8 pin has none)
    // (a space the cut holds but the compile at its pins no longer yields is taken from the cut: the release is
    // immutable and its bundle is what it serves)
    for item in bundle_items(&row.bundle, "context_space") {
        let (Some(key), Some(activity)) = (
            item.get("key").and_then(Value::as_str),
            item.get("activity")
                .and_then(Value::as_str)
                .filter(|a| !a.is_empty()),
        ) else {
            continue;
        };
        match b.context_spaces.get_mut(key) {
            Some(space) => {
                space.insert("activity".to_owned(), Value::String(activity.to_owned()));
            }
            None => {
                b.context_spaces.insert(key.to_owned(), item.clone());
            }
        }
    }
    b.activities = activities_of(
        bundle_items(&row.bundle, "activity"),
        &b.fold_ops,
        &b.context_spaces,
        &b.procedures,
    );
    b.gate_options = bundle_items(&row.bundle, "gate_option");
    Ok(Some(Arc::new(b)))
}

fn languages_of(snap: Option<Value>) -> Vec<String> {
    snap.and_then(|v| serde_json::from_value::<Vec<String>>(v).ok())
        .unwrap_or_default()
}

/// The job's bases for design `of` (module docs). `reg` reads the selection and languages of `Live` / `Draft`
/// (B2's read API, as B4's `RuntimeDesign`); `compiler` compiles the bundle a job never released or a draft would
/// cut (B2's `AppState.compiler`: activities and generated gate options).
pub async fn at(
    c: &mut PgConnection,
    reg: &Registry,
    compiler: &dyn TemplateCompiler,
    job: &JobRef,
    of: &DesignOf,
) -> ApiResult<Arc<Bases>> {
    let release = match of {
        DesignOf::Release(id) => Some(id.clone()),
        DesignOf::Live => sqlx::query_scalar::<_, Option<String>>(
            "SELECT current_release_id FROM jobs WHERE id = $1",
        )
        .bind(job.id)
        .fetch_optional(&mut *c)
        .await?
        .ok_or_else(|| ApiError::Missing("job_not_found", "That job does not exist.".into()))?,
        DesignOf::Draft => None,
    };
    if let Some(id) = release {
        return of_release(&mut *c, job, &id).await?.ok_or_else(|| {
            ApiError::Missing("release_not_found", format!("Release {id} does not exist."))
        });
    }
    // Draft, or Live of a job never released (its baseline)
    // R15: a baseline taken before KH holds the legacy shape; it is upgraded in memory
    let selection = crate::design::read::selection(&mut *c, reg, job, of).await?;
    let languages = languages_of(
        values_of_kind(&mut *c, reg, job, of, "languages")
            .await?
            .into_iter()
            .find_map(|r| r.snap),
    );
    let pins: Value = sqlx::query_scalar("SELECT pins FROM jobs WHERE id = $1")
        .bind(job.id)
        .fetch_optional(&mut *c)
        .await?
        .ok_or_else(|| ApiError::Missing("job_not_found", "That job does not exist.".into()))?;
    let pins: Vec<TemplatePin> = serde_json::from_value(pins)
        .map_err(|e| internal(format!("jobs.pins is unreadable: {e}")))?;
    let mut b = from_compile(&mut *c, pins, selection, languages).await?;
    if !b.template_errors.is_empty() {
        return Ok(Arc::new(b));
    }
    match compiler.compile(&mut *c, &b.selection, &b.pins).await {
        Ok(compiled) => {
            b.activities = activities_of(
                compiled.bundle.objects.activity,
                &b.fold_ops,
                &b.context_spaces,
                &b.procedures,
            );
            b.gate_options = compiled.bundle.objects.gate_option;
        }
        Err(CompileError::Failed(violations)) => {
            b.template_errors = violations.into_iter().map(|v| v.path).collect();
        }
        Err(CompileError::Api(e)) if is_template_refusal(&e) => {
            b.template_errors = vec![refusal_code(&e)];
        }
        Err(CompileError::Api(e)) => return Err(*e),
    }
    if !b.template_errors.is_empty() {
        // the bundle did not compile: no half bases
        b = Bases {
            pins: b.pins,
            selection: b.selection,
            languages: b.languages,
            template_errors: b.template_errors,
            ..Bases::default()
        };
    }
    Ok(Arc::new(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gate(key: &str, options: Value) -> CatalogueItem {
        let mut fields = Map::new();
        fields.insert("options".into(), options);
        CatalogueItem {
            key: key.into(),
            row_id: "r".into(),
            layer: "base".into(),
            fields,
        }
    }

    /// R2: a base context space names the activity it serves in its own item; one that names none serves none.
    #[test]
    fn a_space_s_activity_is_its_own() {
        let mut b = Bases::default();
        let fields = |v: Value| v.as_object().cloned().unwrap();
        b.context_spaces.insert(
            "retail:logistics:track_parcel".into(),
            fields(
                json!({"label": "Where is my parcel", "activity": "delivery:where_is_my_order"}),
            ),
        );
        b.context_spaces
            .insert("retail:faq:other".into(), fields(json!({"label": "Other"})));
        assert_eq!(
            b.space_activity("retail:logistics:track_parcel").as_deref(),
            Some("delivery:where_is_my_order")
        );
        assert_eq!(b.space_activity("retail:faq:other"), None);
        assert_eq!(
            context_space_for(&b, "delivery:where_is_my_order").as_deref(),
            Some("retail:logistics:track_parcel")
        );
        assert_eq!(context_space_for(&b, "support:start_return"), None);
    }

    #[test]
    fn b6_options_are_the_languages_plus_other() {
        let lib = json!([
            {"key": "nl", "label": "Dutch", "description": "", "generated": true},
            {"key": "en", "label": "English", "description": "", "generated": true},
            {"key": "other", "label": "other", "description": "", "generated": true},
        ]);
        let mut gates = vec![gate("B6", lib.clone()), gate("GT", json!([]))];
        apply_languages(&mut gates, &[]);
        assert_eq!(gates[0].fields["options"], lib);
        apply_languages(&mut gates, &["en".to_owned(), "de".to_owned()]);
        let keys: Vec<&str> = gate_options_of(&gates[0])
            .map(|o| o["key"].as_str().unwrap())
            .collect();
        assert_eq!(keys, ["en", "de", "other"]);
        let de = gate_options_of(&gates[0]).nth(1).unwrap();
        assert_eq!(de["label"], "German");
        assert_eq!(de["generated"], true);
        assert_eq!(gates[1].fields["options"], json!([]));
    }

    #[test]
    fn hashes_ignore_what_the_base_does_not_own() {
        let a = json!({"name": "Order", "label": "Order", "attributes": [1]});
        let b = json!({"name": "Order", "label": "Order", "attributes": [2]});
        assert_eq!(
            entity_hash(a.as_object().unwrap()).unwrap(),
            entity_hash(b.as_object().unwrap()).unwrap()
        );
        let mut g = gate("GT", json!([]));
        let h = gate_rule_hash(&g).unwrap();
        g.fields.insert("threshold_value".into(), json!(0.5));
        assert_ne!(gate_rule_hash(&g).unwrap(), h);
    }

    #[test]
    fn an_activity_the_fold_gives_no_ops_takes_them_from_its_space_s_procedure() {
        let obj = |v: Value| v.as_object().cloned().unwrap();
        let mut b = Bases::default();
        b.context_spaces.insert(
            "insurance:claims:claim_report".into(),
            obj(json!({"label": "Claim", "activity": "support:claim_report", "procedure": "claim_report@1"})),
        );
        b.context_spaces.insert(
            "customer_service:identity:identity_check".into(),
            obj(json!({"label": "Identity", "procedure": "identity_check@1"})),
        );
        b.procedures.insert(
            "claim_report@1".into(),
            obj(json!({"label": "Claim", "steps": [
                {"step_number": 1, "name": "who", "operation": null, "variant": null, "side_effect": null},
                {"step_number": 2, "name": "policies", "operation": "policy.list", "variant": null, "side_effect": "R"},
                {"step_number": 3, "name": "policy", "operation": "policy.read", "variant": null, "side_effect": "R"},
                {"step_number": 4, "name": "again", "operation": "policy.read", "variant": null, "side_effect": "R"},
                {"step_number": 5, "name": "open", "operation": "claim.create", "variant": "claim", "side_effect": "W"},
            ]})),
        );
        let ops = derived_ops("support:claim_report", &b.context_spaces, &b.procedures);
        let op = |o: &str, v: Option<&str>, required| ActivityOp {
            op: o.into(),
            variant: v.map(str::to_owned),
            required,
        };
        assert_eq!(
            ops,
            [
                op("policy.list", None, true),
                op("policy.read", None, true),
                op("claim.create", Some("claim"), false),
            ]
        );
        // no space names it: nothing derived
        assert!(derived_ops("support:other", &b.context_spaces, &b.procedures).is_empty());
        // the fold's operations win over the derived ones; an activity the fold gives none derives them
        let item = |k: &str| obj(json!({"key": k}));
        let mut fold = BTreeMap::new();
        fold.insert(
            "change:cancel_order".to_owned(),
            vec![op("order.cancel", None, false)],
        );
        let acts = activities_of(
            vec![item("support:claim_report"), item("change:cancel_order")],
            &fold,
            &b.context_spaces,
            &b.procedures,
        );
        assert_eq!(acts[0].ops, ops);
        assert_eq!(acts[1].ops, [op("order.cancel", None, false)]);
    }

    #[test]
    fn procedure_versions_parse_the_compiled_keys() {
        let mut b = Bases::default();
        for k in ["return@2", "return@1", "warranty@1", "plain"] {
            b.procedures.insert(k.into(), Map::new());
        }
        assert_eq!(b.procedure_versions("return"), [1, 2]);
        assert_eq!(b.procedure_versions("plain"), [1]);
        assert!(b.procedure_versions("none").is_empty());
    }
}
