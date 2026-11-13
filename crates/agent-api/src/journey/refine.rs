//! B8 T7 + T7a: the job's journey refinements (J4/J5) — the ledger kind `activity` and its writes.
//!
//! **One store (Q-R3 (a)).** A tenant's refinement of a journey activity is a row of B2's `job_overlay_row`, object
//! `activity`, key = the activity key (`<stage>:<name>`); there is no table of B8's own. This module is its ONLY
//! writer (through [`crate::release::overlay::write_activity_row`], which wraps B2's `write_overlay`, so the hashes
//! and conflict decisions are J13's); J13's `refine` / `resolveConflict` answer 409 `edit_on_journey_page` for the
//! object. B2's `base_removed` conflict on an activity is F-43's "activity retired".
//!
//! | refinement | row | item |
//! |---|---|---|
//! | hide | `tombstone` over the base | none |
//! | relabel / card (F-44) of a template activity | `override` over the base (`base_hash`) | sparse `{key, label?, card?}` |
//! | extension | `extension` (no base) | the whole activity (`JourneyActivity`) + `card?` |
//!
//! `card` holds only the fields the tenant changed (`summary`, `examples`, `metric`, `sample_dialogue`); `scenarios`
//! are the platform's (422 `scenarios_not_refinable`). A hide replaces a relabel / card on the same activity (one
//! row has one kind); relabelling or editing the card of a hidden activity is 409 `hidden`.
//!
//! **The target.** [`ActivityTarget`] is a thin view of that row for the ledger: key `journey:<key>`, `get` / `set` /
//! `proj` are B2's [`Overlay`] at `{object: "activity", key}` (so a discard, rollback or follow-live restores the
//! row exactly, hashes included), `text` is the fixture's (`as the template has it` / `your activity “…”` /
//! `hidden · “…”`) plus `card refined`.
//!
//! **The writes** are the 7 job-scoped routes of `routes/journey.rs` and B4's `draftHandoverProposal`
//! ([`draft_from_handover`]). Each runs in the caller's job-scoped transaction, validates against the job's DRAFT
//! bases (`job_design::base::at`, the bundle's `objects.activity` with the seed's operations) and the tenant's rows,
//! then writes one tracked row (D54: never live on save). They return the job's `draft_rev`.
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use agent_model::kinds::{Kind, validate_fields};
use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};
use sqlx::PgConnection;

use super::model::{
    ActivityObject, Actor, JourneyActivity, ObjectRole, OperationExpectation, Slug, is_slug,
};
use crate::design::{DesignOf, DesignTarget, DraftRef, JobRef, Place, Registry, Scope, Snap};
use crate::error::{ApiError, ApiResult};
use crate::job_design::base::{self, BaseActivity, Bases};
use crate::release::compile::TemplateCompiler;
use crate::release::overlay::{ACTIVITY, ActivityWrite, base_hash, write_activity_row};
use crate::release::targets::Overlay;
use crate::tenancy::JobCtx;

const TOMBSTONE: &str = "tombstone";
const OVERRIDE: &str = "override";
const EXTENSION: &str = "extension";

/// The fields of a use case card a tenant may refine (F-44); `scenarios` are the platform's.
pub const CARD_FIELDS: [&str; 4] = ["summary", "examples", "metric", "sample_dialogue"];

/// Register the `activity` target (one line in `design::registry()`).
pub fn register(r: &mut Registry) {
    r.register(Arc::new(ActivityTarget));
}

// ---- the ledger target ----------------------------------------------------------------------------------------------

fn key_of(r: &DraftRef) -> &str {
    r.get("key").and_then(Value::as_str).unwrap_or("")
}

/// The overlay place of an `activity` place.
fn overlay_ref(r: &DraftRef) -> DraftRef {
    let mut m = DraftRef::new();
    m.insert("object".to_owned(), Value::String(ACTIVITY.to_owned()));
    m.insert("key".to_owned(), Value::String(key_of(r).to_owned()));
    m
}

/// The tenant's refinement of one journey activity (`job_overlay_row` object `activity`); the value is B2's overlay
/// row `{key, kind, item, base_hash, content_hash, updated_by, updated_at}`.
pub struct ActivityTarget;

impl DesignTarget for ActivityTarget {
    fn kind(&self) -> &'static str {
        "activity"
    }
    fn scope(&self) -> Scope {
        Scope::Job
    }
    fn key(&self, r: &DraftRef) -> String {
        format!("journey:{}", key_of(r))
    }
    fn get<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
        r: &'a DraftRef,
    ) -> BoxFuture<'a, ApiResult<Snap>> {
        Box::pin(async move { Overlay.get(&mut *c, at, &overlay_ref(r)).await })
    }
    fn set<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
        r: &'a DraftRef,
        v: &'a Snap,
    ) -> BoxFuture<'a, ApiResult<()>> {
        Box::pin(async move { Overlay.set(&mut *c, at, &overlay_ref(r), v).await })
    }
    fn places<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
    ) -> BoxFuture<'a, ApiResult<Option<Vec<DraftRef>>>> {
        Box::pin(async move {
            let job = at.job.ok_or_else(|| {
                ApiError::Internal(anyhow::anyhow!(
                    "the activity target is read and written at a job place"
                ))
            })?;
            let keys: Vec<String> = sqlx::query_scalar(
                "SELECT key FROM job_overlay_row WHERE job_id = $1 AND object = $2 ORDER BY key",
            )
            .bind(job.id)
            .bind(ACTIVITY)
            .fetch_all(&mut *c)
            .await?;
            Ok(Some(
                keys.into_iter()
                    .map(|k| {
                        let mut m = DraftRef::new();
                        m.insert("key".to_owned(), Value::String(k));
                        m
                    })
                    .collect(),
            ))
        })
    }
    fn proj(&self, v: &Snap) -> Value {
        Overlay.proj(v)
    }
    fn text(&self, v: &Snap, _r: &DraftRef) -> Option<String> {
        let Some(row) = v.as_ref().filter(|x| !x.is_null()) else {
            return Some("as the template has it".to_owned());
        };
        let kind = row.get("kind").and_then(Value::as_str).unwrap_or("");
        let item = row.get("item");
        let label = item
            .and_then(|i| i.get("label"))
            .and_then(Value::as_str)
            .filter(|l| !l.is_empty());
        let card = item.is_some_and(|i| i.get("card").is_some_and(|c| !c.is_null()));
        if kind == TOMBSTONE {
            return Some("hidden".to_owned());
        }
        let mut parts: Vec<String> = Vec::new();
        if kind == EXTENSION {
            parts.push(match label {
                Some(l) => format!("your activity “{l}”"),
                None => "your activity".to_owned(),
            });
        } else if let Some(l) = label {
            parts.push(format!("“{l}”"));
        }
        if card {
            parts.push("card refined".to_owned());
        }
        Some(parts.join(" · "))
    }
}

// ---- reading the job's activities -----------------------------------------------------------------------------------

/// A tenant's `activity` row, as the writes read it.
struct Row {
    kind: String,
    item: Map<String, Value>,
    base_hash: Option<String>,
}

/// The job's DRAFT bases and the tenant's activity rows, read under the job lock.
struct State {
    bases: Arc<Bases>,
    rows: BTreeMap<String, Row>,
}

async fn load(
    c: &mut PgConnection,
    reg: &Registry,
    compiler: &dyn TemplateCompiler,
    j: &JobCtx,
) -> ApiResult<State> {
    sqlx::query("SELECT 1 FROM jobs WHERE id = $1 FOR UPDATE")
        .bind(j.job_id)
        .execute(&mut *c)
        .await?;
    let jr = JobRef {
        id: j.job_id,
        key: j.job_key.clone(),
    };
    let bases = base::at(&mut *c, reg, compiler, &jr, &DesignOf::Draft).await?;
    let stored: Vec<(String, String, Option<Value>, Option<String>)> = sqlx::query_as(
        "SELECT key, kind, item, base_hash FROM job_overlay_row WHERE job_id = $1 AND object = $2 ORDER BY key",
    )
    .bind(j.job_id)
    .bind(ACTIVITY)
    .fetch_all(&mut *c)
    .await?;
    let rows = stored
        .into_iter()
        .map(|(key, kind, item, base_hash)| {
            let item = match item {
                Some(Value::Object(m)) => m,
                _ => Map::new(),
            };
            (
                key,
                Row {
                    kind,
                    item,
                    base_hash,
                },
            )
        })
        .collect();
    Ok(State { bases, rows })
}

fn activity_not_found(key: &str) -> ApiError {
    ApiError::Missing("activity_not_found", format!("No template activity {key}."))
}

fn hidden(key: &str) -> ApiError {
    ApiError::Conflict("hidden", format!("{key} is hidden; restore it first."))
}

fn not_refined(what: String) -> ApiError {
    ApiError::Conflict("not_refined", what)
}

fn base_label(a: &BaseActivity) -> Option<&str> {
    a.item.get("label").and_then(Value::as_str)
}

/// `object.verb{variant}` (fixture `opLabel`).
fn op_label(op: &str, variant: Option<&str>) -> String {
    match variant {
        Some(v) => format!("{op}{{{v}}}"),
        None => op.to_owned(),
    }
}

fn required_ops_of_base(a: &BaseActivity) -> Vec<String> {
    a.ops
        .iter()
        .filter(|o| o.required)
        .map(|o| op_label(&o.op, o.variant.as_deref()))
        .collect()
}

/// The required operations an extension item declares (`operations: [{op, variant, required}]`).
fn required_ops_of_item(item: &Map<String, Value>) -> Vec<String> {
    item.get("operations")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|o| o.get("required") == Some(&Value::Bool(true)))
        .filter_map(|o| {
            let op = o.get("op")?.as_str()?;
            Some(op_label(op, o.get("variant").and_then(Value::as_str)))
        })
        .collect()
}

/// A sparse override that has nothing left but its key is no refinement: the row goes.
fn sparse(item: Map<String, Value>, base_hash: String) -> ActivityWrite {
    if item.keys().all(|k| k == "key") {
        ActivityWrite::Remove
    } else {
        ActivityWrite::Override {
            item: Value::Object(item),
            base_hash,
        }
    }
}

fn only_key(key: &str) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("key".to_owned(), Value::String(key.to_owned()));
    m
}

/// SEAM (T5): is `stage` out of scope for this job — the effective `stage_scope` rows of its pinned department
/// layers (D107)? The job journey's fold answers it; until that lands no stage is out of scope, so
/// `stage_out_of_scope` cannot fire yet.
async fn stage_out_of_scope(
    _c: &mut PgConnection,
    _bases: &Bases,
    _stage: &str,
) -> ApiResult<bool> {
    Ok(false)
}

// ---- hide / restore -------------------------------------------------------------------------------------------------

/// `hideJobActivity`: tombstone a template activity. Refusals: 404 `activity_not_found`; 422 `is_extension` (your
/// own activity: remove it), `required_activity` (the template marks it required), `required_operation` (K48: a
/// required operation of it would be covered by no other enabled activity or extension); 409 `already_hidden`.
pub async fn hide(
    c: &mut PgConnection,
    reg: &Registry,
    compiler: &dyn TemplateCompiler,
    j: &JobCtx,
    key: &str,
) -> ApiResult<i64> {
    let s = load(c, reg, compiler, j).await?;
    let current = s.rows.get(key);
    let Some(target) = s.bases.activity(key) else {
        if current.is_some_and(|r| r.kind == EXTENSION) {
            return Err(ApiError::Unprocessable(
                "is_extension",
                "This is your own extension: remove it instead of hiding it.".into(),
            ));
        }
        return Err(activity_not_found(key));
    };
    if current.is_some_and(|r| r.kind == TOMBSTONE) {
        return Err(ApiError::Conflict(
            "already_hidden",
            format!("{key} is already hidden."),
        ));
    }
    if target.item.get("required") == Some(&Value::Bool(true)) {
        return Err(ApiError::Unprocessable(
            "required_activity",
            format!("“{key}” is required by the job template and cannot be hidden."),
        ));
    }
    // K48: every required operation of the activity stays covered by another enabled activity or an extension
    let mut covered: BTreeSet<String> = BTreeSet::new();
    for a in s.bases.activities.iter().filter(|a| a.key != key) {
        if s.rows.get(&a.key).is_some_and(|r| r.kind == TOMBSTONE) {
            continue;
        }
        covered.extend(required_ops_of_base(a));
    }
    for r in s.rows.values().filter(|r| r.kind == EXTENSION) {
        covered.extend(required_ops_of_item(&r.item));
    }
    let lost: Vec<String> = required_ops_of_base(target)
        .into_iter()
        .filter(|o| !covered.contains(o))
        .collect();
    if !lost.is_empty() {
        return Err(ApiError::Unprocessable(
            "required_operation",
            format!(
                "Hiding {key} would drop required operation {} (K48).",
                lost.join(", ")
            ),
        ));
    }
    let hash = base_hash(ACTIVITY, &target.item)?;
    write_activity_row(
        c,
        reg,
        j,
        key,
        format!("hid {key}"),
        ActivityWrite::Tombstone { base_hash: hash },
        None,
    )
    .await
}

/// `restoreJobActivity`: undo the hide first, else reset the relabel. 409 `not_refined` when there is neither.
pub async fn restore(
    c: &mut PgConnection,
    reg: &Registry,
    compiler: &dyn TemplateCompiler,
    j: &JobCtx,
    key: &str,
) -> ApiResult<i64> {
    let s = load(c, reg, compiler, j).await?;
    match s.rows.get(key) {
        Some(r) if r.kind == TOMBSTONE => {
            write_activity_row(
                c,
                reg,
                j,
                key,
                format!("restored {key}"),
                ActivityWrite::Remove,
                Some("reverted"),
            )
            .await
        }
        Some(r) if r.kind == OVERRIDE && r.item.contains_key("label") => {
            let mut item = r.item.clone();
            item.remove("label");
            let w = sparse(item, r.base_hash.clone().unwrap_or_default());
            let choice = matches!(w, ActivityWrite::Remove).then_some("reverted");
            write_activity_row(c, reg, j, key, format!("reset label of {key}"), w, choice).await
        }
        _ => Err(not_refined(format!(
            "{key} has no hide or relabel to undo."
        ))),
    }
}

// ---- extensions -----------------------------------------------------------------------------------------------------

/// `addJobExtensionActivity` body (fixture `addJobExtensionActivity` input).
#[derive(Debug, serde::Deserialize)]
pub struct NewExtension {
    pub stage: String,
    pub slug: String,
    pub label: String,
    pub actor: Actor,
    #[serde(default)]
    pub trigger: String,
}

/// The extension every new tenant activity starts as: it reads the knowledge base (fixture): the `knowledge`
/// object (`article`, `passages`, `citation`) and the required operation `knowledge.search`.
fn extension_item(
    key: &str,
    stage: Slug,
    actor: Actor,
    label: &str,
    trigger: &str,
    objects: Vec<ActivityObject>,
    operations: Vec<OperationExpectation>,
) -> ApiResult<Value> {
    let a = JourneyActivity {
        key: key.to_owned(),
        stage,
        actor,
        label: label.to_owned(),
        trigger: trigger.to_owned(),
        also_in_stages: Vec::new(),
        objects,
        operations,
        summary: None,
        description: None,
        area: None,
        layer: None,
        required: false,
    };
    serde_json::to_value(&a).map_err(|e| {
        ApiError::Internal(anyhow::anyhow!(
            "an extension activity does not serialise: {e}"
        ))
    })
}

fn knowledge_defaults() -> (Vec<ActivityObject>, Vec<OperationExpectation>) {
    (
        vec![ActivityObject {
            object: "knowledge".to_owned(),
            role: ObjectRole::Subject,
            fields: vec![
                "article".to_owned(),
                "passages".to_owned(),
                "citation".to_owned(),
            ],
            required_inputs: Vec::new(),
        }],
        vec![OperationExpectation {
            op: "knowledge.search".to_owned(),
            variant: None,
            required: true,
        }],
    )
}

/// `addJobExtensionActivity`: the tenant's own activity `<stage>:<slug>`. Refusals: 422 `invalid_key` (stage or
/// slug is not `^[a-z][a-z0-9_]{1,40}$`), `label_required`, `stage_out_of_scope`; 409 `activity_exists`.
pub async fn add_extension(
    c: &mut PgConnection,
    reg: &Registry,
    compiler: &dyn TemplateCompiler,
    j: &JobCtx,
    b: NewExtension,
) -> ApiResult<i64> {
    let slug = b.slug.trim();
    let stage = b.stage.trim();
    if !is_slug(slug) {
        return Err(ApiError::Unprocessable(
            "invalid_key",
            "Use lower_snake_case for the activity key (e.g. assembly_help).".into(),
        ));
    }
    let Some(stage_key) = Slug::new(stage) else {
        return Err(ApiError::Unprocessable(
            "invalid_key",
            format!("“{stage}” is not a stage key."),
        ));
    };
    let label = b.label.trim();
    if label.is_empty() {
        return Err(ApiError::Unprocessable(
            "label_required",
            "Give the activity a label.".into(),
        ));
    }
    let s = load(c, reg, compiler, j).await?;
    if stage_out_of_scope(c, &s.bases, stage).await? {
        return Err(ApiError::Unprocessable(
            "stage_out_of_scope",
            format!("{stage} is out of scope for this job."),
        ));
    }
    let key = format!("{stage}:{slug}");
    if s.bases.activity(&key).is_some() || s.rows.contains_key(&key) {
        return Err(ApiError::Conflict(
            "activity_exists",
            format!("{key} already exists."),
        ));
    }
    let (objects, operations) = knowledge_defaults();
    let item = extension_item(
        &key,
        stage_key,
        b.actor,
        label,
        b.trigger.trim(),
        objects,
        operations,
    )?;
    write_activity_row(
        c,
        reg,
        j,
        &key,
        format!("added extension {key}"),
        ActivityWrite::Extension { item },
        None,
    )
    .await
}

/// `removeJobExtensionActivity`: drop the tenant's own activity. 404 `extension_not_found`.
pub async fn remove_extension(
    c: &mut PgConnection,
    reg: &Registry,
    compiler: &dyn TemplateCompiler,
    j: &JobCtx,
    key: &str,
) -> ApiResult<i64> {
    let s = load(c, reg, compiler, j).await?;
    if !s.rows.get(key).is_some_and(|r| r.kind == EXTENSION) {
        return Err(ApiError::Missing(
            "extension_not_found",
            format!("{key} is not a tenant extension."),
        ));
    }
    write_activity_row(
        c,
        reg,
        j,
        key,
        format!("removed extension {key}"),
        ActivityWrite::Remove,
        None,
    )
    .await
}

// ---- relabel --------------------------------------------------------------------------------------------------------

/// `relabelJobActivity`: a template activity gets a tenant label (an override); an extension's own label changes.
/// A label equal to the template's drops the relabel. Refusals: 422 `label_required`; 404 `activity_not_found`;
/// 409 `hidden`.
pub async fn relabel(
    c: &mut PgConnection,
    reg: &Registry,
    compiler: &dyn TemplateCompiler,
    j: &JobCtx,
    key: &str,
    label: &str,
) -> ApiResult<i64> {
    let text = label.trim();
    if text.is_empty() {
        return Err(ApiError::Unprocessable(
            "label_required",
            "A label cannot be empty.".into(),
        ));
    }
    let s = load(c, reg, compiler, j).await?;
    let what = format!("relabelled {key} → “{text}”");
    match s.rows.get(key) {
        Some(r) if r.kind == EXTENSION => {
            let mut item = r.item.clone();
            item.insert("label".to_owned(), Value::String(text.to_owned()));
            write_activity_row(
                c,
                reg,
                j,
                key,
                what,
                ActivityWrite::Extension {
                    item: Value::Object(item),
                },
                None,
            )
            .await
        }
        row => {
            let Some(target) = s.bases.activity(key) else {
                return Err(ApiError::Missing(
                    "activity_not_found",
                    format!("No activity {key}."),
                ));
            };
            if row.is_some_and(|r| r.kind == TOMBSTONE) {
                return Err(hidden(key));
            }
            let mut item = row.map_or_else(|| only_key(key), |r| r.item.clone());
            if base_label(target) == Some(text) {
                item.remove("label");
            } else {
                item.insert("label".to_owned(), Value::String(text.to_owned()));
            }
            let hash = base_hash(ACTIVITY, &target.item)?;
            let w = sparse(item, hash);
            let choice = matches!(w, ActivityWrite::Remove).then_some("reverted");
            write_activity_row(c, reg, j, key, what, w, choice).await
        }
    }
}

// ---- the use case card (T7a, F-44) -----------------------------------------------------------------------------------

/// Validate a card refinement body: 422 `scenarios_not_refinable` for `scenarios`, 422 `invalid_card` for another
/// field than [`CARD_FIELDS`], an empty body, or a value the `use_case_card` kind refuses. A `null` field drops that
/// field of the tenant's card. Returns the non-null fields and the nulls (fields to drop).
fn card_patch(body: &Map<String, Value>) -> ApiResult<(Map<String, Value>, Vec<String>)> {
    if body.contains_key("scenarios") {
        return Err(ApiError::Unprocessable(
            "scenarios_not_refinable",
            "Scenarios belong to the platform's use case card; a job cannot refine them.".into(),
        ));
    }
    if let Some(f) = body.keys().find(|k| !CARD_FIELDS.contains(&k.as_str())) {
        return Err(ApiError::Unprocessable(
            "invalid_card",
            format!("A card has {} (not “{f}”).", CARD_FIELDS.join(", ")),
        ));
    }
    if body.is_empty() {
        return Err(ApiError::Unprocessable(
            "invalid_card",
            "Nothing to change.".into(),
        ));
    }
    let mut set = Map::new();
    let mut unset = Vec::new();
    for (k, v) in body {
        if v.is_null() {
            unset.push(k.clone());
        } else {
            set.insert(k.clone(), v.clone());
        }
    }
    if let Err(errors) = validate_fields(Kind::UseCaseCard, &Value::Object(set.clone())) {
        let first = errors
            .first()
            .map(|e| match &e.field {
                Some(f) => format!("{f}: {}", e.message),
                None => e.message.clone(),
            })
            .unwrap_or_default();
        return Err(ApiError::Unprocessable(
            "invalid_card",
            format!("The card is not valid: {first}"),
        ));
    }
    Ok((set, unset))
}

/// `item.card` after `set` / `unset` are applied to it (an empty card is no card).
fn apply_card(item: &mut Map<String, Value>, set: Map<String, Value>, unset: &[String]) {
    let mut card: Map<String, Value> = item
        .get("card")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    for k in unset {
        card.remove(k);
    }
    card.extend(set);
    if card.is_empty() {
        item.remove("card");
    } else {
        item.insert("card".to_owned(), Value::Object(card));
    }
}

/// `refineJobUseCaseCard`: the tenant's changes to one activity's card, a draft (D54). Refusals: 422
/// `scenarios_not_refinable`, `invalid_card`; 404 `activity_not_found`; 409 `hidden`.
pub async fn refine_card(
    c: &mut PgConnection,
    reg: &Registry,
    compiler: &dyn TemplateCompiler,
    j: &JobCtx,
    key: &str,
    body: &Map<String, Value>,
) -> ApiResult<i64> {
    let (set, unset) = card_patch(body)?;
    let s = load(c, reg, compiler, j).await?;
    let what = format!("refined the use case card of {key}");
    match s.rows.get(key) {
        Some(r) if r.kind == TOMBSTONE => Err(hidden(key)),
        Some(r) if r.kind == EXTENSION => {
            let mut item = r.item.clone();
            apply_card(&mut item, set, &unset);
            write_activity_row(
                c,
                reg,
                j,
                key,
                what,
                ActivityWrite::Extension {
                    item: Value::Object(item),
                },
                None,
            )
            .await
        }
        row => {
            let Some(target) = s.bases.activity(key) else {
                return Err(activity_not_found(key));
            };
            let mut item = row.map_or_else(|| only_key(key), |r| r.item.clone());
            apply_card(&mut item, set, &unset);
            let hash = base_hash(ACTIVITY, &target.item)?;
            let w = sparse(item, hash);
            let choice = matches!(w, ActivityWrite::Remove).then_some("reverted");
            write_activity_row(c, reg, j, key, what, w, choice).await
        }
    }
}

/// `resetJobUseCaseCard`: back to the template's card. 409 `not_refined` when the tenant has no card for it.
pub async fn reset_card(
    c: &mut PgConnection,
    reg: &Registry,
    compiler: &dyn TemplateCompiler,
    j: &JobCtx,
    key: &str,
) -> ApiResult<i64> {
    let s = load(c, reg, compiler, j).await?;
    let what = format!("reset the use case card of {key}");
    match s.rows.get(key) {
        Some(r) if r.kind != TOMBSTONE && r.item.contains_key("card") => {
            let mut item = r.item.clone();
            item.remove("card");
            if r.kind == EXTENSION {
                write_activity_row(
                    c,
                    reg,
                    j,
                    key,
                    what,
                    ActivityWrite::Extension {
                        item: Value::Object(item),
                    },
                    None,
                )
                .await
            } else {
                let w = sparse(item, r.base_hash.clone().unwrap_or_default());
                let choice = matches!(w, ActivityWrite::Remove).then_some("reverted");
                write_activity_row(c, reg, j, key, what, w, choice).await
            }
        }
        _ => Err(not_refined(format!("{key} has no refined card to reset."))),
    }
}

// ---- B4's draftHandoverProposal (D64) -----------------------------------------------------------------------------------

fn proposal_text<'p>(proposal: &'p Value, field: &str) -> Option<&'p str> {
    proposal
        .get(field)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// Save a handover item's proposal (`enable_activity` / `tenant_extension`) to the draft as the J4 write it stands
/// for. `proposal` is the item's feedback proposal `{kind, activity, stage?, label?, objects?, operations?}`; the
/// caller has checked that its kind is draftable and that it names an activity. Refusals: 409 `already_enabled` (the
/// activity is not hidden), `already_extended` (the key is a template activity or already the tenant's).
pub async fn draft_from_handover(
    c: &mut PgConnection,
    reg: &Registry,
    compiler: &dyn TemplateCompiler,
    j: &JobCtx,
    proposal: &Value,
    subject: &str,
    item_id: &str,
) -> ApiResult<i64> {
    let text = |f: &str| proposal_text(proposal, f);
    let key = text("activity").unwrap_or_default().to_owned();
    let s = load(c, reg, compiler, j).await?;
    if text("kind") == Some("enable_activity") {
        if !s.rows.get(&key).is_some_and(|r| r.kind == TOMBSTONE) {
            return Err(ApiError::Conflict(
                "already_enabled",
                format!("{key} is already enabled."),
            ));
        }
        return write_activity_row(
            c,
            reg,
            j,
            &key,
            format!("enabled {key} (handover {item_id})"),
            ActivityWrite::Remove,
            Some("reverted"),
        )
        .await;
    }
    if s.bases.activity(&key).is_some() || s.rows.contains_key(&key) {
        return Err(ApiError::Conflict(
            "already_extended",
            format!("{key} is already one of your activities."),
        ));
    }
    let stage = text("stage")
        .and_then(Slug::new)
        .or_else(|| Slug::new("support"))
        .ok_or_else(|| ApiError::Internal(anyhow::anyhow!("“support” is not a stage key")))?;
    let list = |f: &str| proposal.get(f).cloned().unwrap_or_else(|| json!([]));
    let objects: Vec<ActivityObject> = serde_json::from_value(list("objects")).unwrap_or_default();
    let operations: Vec<OperationExpectation> =
        serde_json::from_value(list("operations")).unwrap_or_default();
    let label = text("label").unwrap_or(&key).to_owned();
    let item = extension_item(
        &key,
        stage,
        Actor::Customer,
        &label,
        &format!("handover {item_id}: {subject}"),
        objects,
        operations,
    )?;
    write_activity_row(
        c,
        reg,
        j,
        &key,
        format!("added extension {key} (handover {item_id})"),
        ActivityWrite::Extension { item },
        None,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(kind: &str, item: Value) -> Snap {
        Some(json!({"key": "support:a", "kind": kind, "item": item}))
    }

    fn r() -> DraftRef {
        overlay_ref(&{
            let mut m = DraftRef::new();
            m.insert("key".into(), json!("support:a"));
            m
        })
    }

    #[test]
    fn key_and_text_follow_the_fixture() {
        let t = ActivityTarget;
        let mut m = DraftRef::new();
        m.insert("key".into(), json!("support:a"));
        assert_eq!(t.key(&m), "journey:support:a");
        assert_eq!(r()["object"], json!("activity"));
        assert_eq!(t.text(&None, &m).as_deref(), Some("as the template has it"));
        assert_eq!(
            t.text(&snap(TOMBSTONE, Value::Null), &m).as_deref(),
            Some("hidden")
        );
        assert_eq!(
            t.text(
                &snap(OVERRIDE, json!({"key": "support:a", "label": "Mine"})),
                &m
            )
            .as_deref(),
            Some("“Mine”")
        );
        assert_eq!(
            t.text(
                &snap(
                    OVERRIDE,
                    json!({"key": "support:a", "card": {"summary": "s"}})
                ),
                &m
            )
            .as_deref(),
            Some("card refined")
        );
        assert_eq!(
            t.text(
                &snap(
                    EXTENSION,
                    json!({"key": "support:a", "label": "Gifts", "card": {"summary": "s"}})
                ),
                &m
            )
            .as_deref(),
            Some("your activity “Gifts” · card refined")
        );
    }

    #[test]
    fn card_patch_rules() {
        let body = |v: Value| v.as_object().cloned().unwrap();
        let err = |v: Value| match card_patch(&body(v)).unwrap_err() {
            ApiError::Unprocessable(code, _) => code,
            other => panic!("{other:?}"),
        };
        assert_eq!(err(json!({"scenarios": []})), "scenarios_not_refinable");
        assert_eq!(err(json!({"label": "x"})), "invalid_card");
        assert_eq!(err(json!({})), "invalid_card");
        assert_eq!(err(json!({"metric": "90%"})), "invalid_card");
        let (set, unset) = card_patch(&body(json!({"summary": "s", "examples": null}))).unwrap();
        assert_eq!(set["summary"], json!("s"));
        assert_eq!(unset, ["examples"]);
    }

    #[test]
    fn apply_card_merges_and_drops() {
        let mut item = only_key("support:a");
        apply_card(
            &mut item,
            body_of(json!({"summary": "s", "examples": ["e"]})),
            &[],
        );
        apply_card(
            &mut item,
            body_of(json!({"summary": "t"})),
            &["examples".into()],
        );
        assert_eq!(item["card"], json!({"summary": "t"}));
        apply_card(&mut item, Map::new(), &["summary".into()]);
        assert!(!item.contains_key("card"));
        assert!(matches!(sparse(item, "h".into()), ActivityWrite::Remove));
    }

    fn body_of(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap()
    }
}
