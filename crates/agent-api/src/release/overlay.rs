//! Overlay refinements (B2 T8, D8/D54, 05 §2.5): the tenant's rows over the five refinable objects of a job's bundle
//! (`job_overlay_row`: override / extension / tombstone) and the job-setting refinements (`job_setting_override`).
//! A port of the fixture `refinementLayers`, `refineIn` / `refineRaw` and `saveContextSpaceOverride`
//! (`web/src/lib/api/areas/release.ts`), which is the accepted contract for codes, messages and shapes.
//!
//! - The **base** of an object is the DRAFT-compiled bundle's `objects[object]` (the job's draft selection over its
//!   pins). A job with no selection chosen, or whose selection does not compile (422 `compile_failed`, or a template
//!   refusal, `design::hooks::is_template_refusal`), has no base; any other error propagates.
//! - Every write goes through `design::track` (D54: saved to the draft, never live on save): kind `overlay` (page
//!   J13, J12 for a context space) or `setting` (page J13). Nothing here bumps `jobs.draft_rev` (`track` does).
//! - Hashes are `release::hash::row_hash` of the item (`base_hash` of the base item, `content_hash` of the tenant's
//!   item; a tombstone's `content_hash` is `tomb-<base_hash>`).
//! - Fixture `resolveConflicts` / `linkDecisions`: an unhide / revert (`reverted`) or rebase (`rebased`) saves the
//!   choice on every open conflict of the place (`release_conflict.decision`), and after the tracked write each open
//!   decision of the place points at the ledger row that now carries it (`draft_id` = `d-<id>`, or null).
//! - PK-2: object `knowledge_pack` (key `<stage>|<kind>|<pack_key>`, no version) is a job-level pack exclusion and
//!   takes tombstones only: a write that would leave an `override` / `extension` row is 422 `invalid_refinement`.
//!   Inside the same tracked write, the template-sourced pins of the kinds the key covers follow the template minus
//!   the job's tombstones again (`kinds::refresh_template_packs`).
//! - KH T4 (D108, D8): object `instruction` (key = the prompt slot) refines the release's folded instructions. Only
//!   the slots `tone`, `escalation` and `format` are refinable; any other key (`identity`, `policy`, `safety`, or
//!   no slot at all) is 422 `instruction_not_refinable` on every write. `PUT /api/jobs/{job}/instructions/{slot}`
//!   ([`save_instruction`]) writes the tenant's own text (`{key, text, language?}`) as an override of the base slot,
//!   or an extension when the release has none.
use std::collections::BTreeSet;

use agent_model::compile::Selection;
use agent_model::kinds::{self as model_kinds, Kind, Op as RowOp, instruction};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use super::bundle::{Bundle, OVERLAY_OBJECTS, OverlayItem, base_projection};
use super::compile::{CompileError, TemplateCompiler};
use super::hash::{bundle_hash, row_hash};
use super::targets::SETTING_KEYS;
use crate::design::hooks::is_template_refusal;
use crate::design::{self, DesignOf, DraftRef, DraftWrite, JobRef, Label, Registry};
use crate::error::{ApiError, ApiResult};
use crate::knowledge::kinds;
use crate::platform::templates::TemplatePin;
use crate::tenancy::JobCtx;

/// The refinable object whose rows are the job settings (fixture `RefinableObject`), not overlay rows.
pub const JOB_SETTING: &str = "job_setting";
/// The refinable object of the job's journey activities. B8 Q-R3 (a): the ONE store of the tenant's activity
/// refinements; its only writer is the J4 journey page ([`write_activity_row`]), J13 shows it read-only.
pub const ACTIVITY: &str = "activity";
/// The object `saveContextSpaceOverride` writes.
pub const CONTEXT_SPACE: &str = "context_space";
/// The object B5's `refineGateOption` writes for a generated option (key `<GATE> · <option>`, B5 plan R2).
pub const GATE_OPTION: &str = "gate_option";
/// A context space's label is at most this many UTF-16 units (fixture `saveContextSpaceOverride` `.length`).
pub const MAX_LABEL_CHARS: usize = 120;

const TOMBSTONE: &str = "tombstone";
/// PK-2's object: tombstones only.
pub const KNOWLEDGE_PACK: &str = "knowledge_pack";
/// D108's object: the release's folded prompt slots; tenants refine `tone`, `escalation` and `format` only.
pub const INSTRUCTION: &str = "instruction";
/// A tenant instruction text is at most this many UTF-16 units.
pub const MAX_INSTRUCTION_CHARS: usize = 4000;

/// A refinement operation (fixture `RefinementOp`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Hide,
    Unhide,
    Revert,
    Rebase,
}

impl Op {
    /// `hide | unhide | revert | rebase`, else 422 `invalid_op`.
    pub fn parse(s: &str) -> ApiResult<Op> {
        match s {
            "hide" => Ok(Op::Hide),
            "unhide" => Ok(Op::Unhide),
            "revert" => Ok(Op::Revert),
            "rebase" => Ok(Op::Rebase),
            other => Err(ApiError::Unprocessable(
                "invalid_op",
                format!("No refinement “{other}”; use hide, unhide, revert or rebase."),
            )),
        }
    }

    /// Fixture `OP_DONE`: the ledger label's verb.
    pub fn done(self) -> &'static str {
        match self {
            Op::Hide => "hidden",
            Op::Unhide => "shown again",
            Op::Revert => "back to the release value",
            Op::Rebase => "kept on the new base",
        }
    }
}

/// B5 `refineGateOption` of a GENERATED gate option (B5 plan R2, T8): `{label?, description?, hidden?}`. An absent
/// (or null) field is unchanged; `description: ""` drops the tenant's description.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GateOptionPatch {
    pub label: Option<String>,
    pub description: Option<String>,
    pub hidden: Option<bool>,
}

/// `PUT /api/jobs/{job}/context-spaces/{key}` (fixture `ContextSpacePatch`). An absent (or null) `label` /
/// `entities` is unchanged. `required` (B5 T7, J12 `saveContextSpaceRequired`): absent keeps it, `null` drops it from
/// the tenant's item (the space runs the template's slots again), a list sets it.
#[derive(Debug, Default, Deserialize)]
pub struct ContextSpacePatch {
    pub label: Option<String>,
    #[serde(default, deserialize_with = "present")]
    pub required: Option<Option<Vec<String>>>,
    pub entities: Option<Vec<String>>,
}

/// A field that may be absent (`None`) or null (`Some(None)`).
fn present<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}

impl ContextSpacePatch {
    /// A patch of `required` only (J12): recorded under B5's projection kind `space_required`, not `overlay`.
    pub fn required_only(&self) -> bool {
        self.label.is_none() && self.entities.is_none() && self.required.is_some()
    }
}

/// J12 slots (fixture `saveContextSpaceRequired`): trimmed, blanks dropped; 422 `invalid_slot` for one that is not
/// snake_case, 422 `duplicate_slot` for a repeat.
pub fn j12_slots(required: &[String]) -> ApiResult<Vec<String>> {
    let slots: Vec<String> = required
        .iter()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .collect();
    if slots.iter().any(|s| !is_slot(s)) {
        return Err(ApiError::Unprocessable(
            "invalid_slot",
            "A slot is snake_case, e.g. order_id.".into(),
        ));
    }
    let unique: BTreeSet<&str> = slots.iter().map(String::as_str).collect();
    if unique.len() != slots.len() {
        return Err(ApiError::Unprocessable(
            "duplicate_slot",
            "Slots must be unique.".into(),
        ));
    }
    Ok(slots)
}

// ---- helpers --------------------------------------------------------------------------------------------------------

fn internal(what: String) -> ApiError {
    ApiError::Internal(anyhow::anyhow!(what))
}

fn unknown_object(object: &str) -> ApiError {
    ApiError::Missing("unknown_object", format!("No refinable object “{object}”."))
}

/// 422 `invalid_refinement`: a `knowledge_pack` refinement that is not a tombstone (PK-2).
fn invalid_refinement(key: &str) -> ApiError {
    ApiError::Unprocessable(
        "invalid_refinement",
        format!(
            "A knowledge pack “{key}” can only be hidden (a tombstone); it cannot be overridden or extended."
        ),
    )
}

/// 422 `instruction_not_refinable`: a tenant write to an instruction slot other than `tone`, `escalation`,
/// `format` (D108).
fn not_refinable(slot: &str) -> ApiError {
    ApiError::Unprocessable(
        "instruction_not_refinable",
        format!(
            "The instruction “{slot}” cannot be refined; a job may refine only {}.",
            instruction::TENANT_SLOTS.join(", ")
        ),
    )
}

/// Refuse a tenant write to `key` of `object` that D108 does not allow (`instruction` slots).
fn check_refinable(object: &str, key: &str) -> ApiResult<()> {
    if object == INSTRUCTION && !instruction::tenant_refinable(key) {
        return Err(not_refinable(key));
    }
    Ok(())
}

/// Whether a write leaves no row, or a tombstone (all a `knowledge_pack` place may hold).
fn tombstone_only(change: &RowChange) -> bool {
    match change {
        RowChange::Tombstone { .. } | RowChange::Remove => true,
        RowChange::Rebase { kind, .. } => kind == TOMBSTONE,
        RowChange::Content { .. } | RowChange::Override { .. } | RowChange::Extension { .. } => {
            false
        }
    }
}

fn now_iso() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::AutoSi, true)
}

fn iso(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::AutoSi, true)
}

/// Fixture `SLOT_RE` `^[a-z][a-z0-9_]{0,63}$`.
fn is_slot(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 64
        && b[0].is_ascii_lowercase()
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_')
}

/// The `row_hash` of one overlay item (fixture `itemHash`).
pub fn item_hash(item: &OverlayItem) -> ApiResult<String> {
    row_hash(&Value::Object(item.clone()), &[])
        .map_err(|e| internal(format!("an overlay item does not hash: {e}")))
}

/// The `base_hash` of base item `item` of `object`: the [`item_hash`] of its [`base_projection`] (the conflict
/// detector compares the same).
pub fn base_hash(object: &str, item: &OverlayItem) -> ApiResult<String> {
    item_hash(&base_projection(object, item))
}

/// The hash of a setting's base value (`job_setting_override.base_hash`).
fn setting_hash(v: &str) -> ApiResult<String> {
    bundle_hash(&Value::String(v.to_owned()))
        .map_err(|e| internal(format!("a setting value does not hash: {e}")))
}

fn place_ref(pairs: &[(&str, &str)]) -> DraftRef {
    let mut m = DraftRef::new();
    for (k, v) in pairs {
        m.insert((*k).to_owned(), Value::String((*v).to_owned()));
    }
    m
}

fn job_ref(j: &JobCtx) -> JobRef {
    JobRef {
        id: j.job_id,
        key: j.job_key.clone(),
    }
}

/// Lock the job row (as `design::track` does) so the checks below and the tracked write see one state.
async fn lock_job(c: &mut PgConnection, job: Uuid) -> ApiResult<()> {
    sqlx::query("SELECT 1 FROM jobs WHERE id = $1 FOR UPDATE")
        .bind(job)
        .execute(&mut *c)
        .await?;
    Ok(())
}

/// The bundle the job's DRAFT selection compiles to over its pins; `None` without a selection, or when the selection
/// does not compile (422 `compile_failed`, or a template refusal). Any other error propagates.
pub async fn draft_bundle(
    c: &mut PgConnection,
    compiler: &dyn TemplateCompiler,
    job: Uuid,
) -> ApiResult<Option<Bundle>> {
    let stored: Option<Option<Value>> =
        sqlx::query_scalar("SELECT selection FROM job_target WHERE job_id = $1")
            .bind(job)
            .fetch_optional(&mut *c)
            .await?;
    let Some(sel) = stored.flatten() else {
        return Ok(None);
    };
    let sel: Selection = serde_json::from_value(sel)
        .map_err(|e| internal(format!("job_target.selection is unreadable: {e}")))?;
    let pins: Value = sqlx::query_scalar("SELECT pins FROM jobs WHERE id = $1")
        .bind(job)
        .fetch_one(&mut *c)
        .await?;
    let pins: Vec<TemplatePin> = serde_json::from_value(pins)
        .map_err(|e| internal(format!("jobs.pins is unreadable: {e}")))?;
    match compiler.compile(&mut *c, &sel, &pins).await {
        Ok(compiled) => Ok(Some(compiled.bundle)),
        Err(CompileError::Api(e)) if !is_template_refusal(&e) => Err(*e),
        Err(_) => Ok(None),
    }
}

/// The base item `key` of `object` in `bundle`.
fn base_item<'b>(bundle: &'b Bundle, object: &str, key: &str) -> Option<&'b OverlayItem> {
    bundle
        .objects
        .get(object)?
        .iter()
        .find(|i| i.get("key").and_then(Value::as_str) == Some(key))
}

/// The entity names a context space may reference (fixture `entities`): the bundle's ontology base — the entity of
/// every `ontology_attribute` (`<Entity>.<attribute>`) and every entity a base context space names — and, once B5
/// registers the `entity` kind, the names of the job's DRAFT `entity` places (tenant entities and refined base
/// entities; bases are never materialised as places, B5 plan R1, so the bundle's names stay in).
pub async fn entities(
    c: &mut PgConnection,
    reg: &Registry,
    job: &JobRef,
    bundle: Option<&Bundle>,
) -> ApiResult<Vec<String>> {
    let mut names: BTreeSet<String> = BTreeSet::new();
    if reg.kinds().contains(&"entity") {
        for row in design::values_of_kind(c, reg, job, &DesignOf::Draft, "entity").await? {
            if row.snap.as_ref().is_some_and(|v| !v.is_null())
                && let Some(n) = row.r#ref.get("name").and_then(Value::as_str)
            {
                names.insert(n.to_owned());
            }
        }
    }
    if let Some(b) = bundle {
        for a in &b.objects.ontology_attribute {
            if let Some((entity, _)) = a
                .get("key")
                .and_then(Value::as_str)
                .and_then(|k| k.split_once('.'))
            {
                names.insert(entity.to_owned());
            }
        }
        for s in &b.objects.context_space {
            for e in s
                .get("entities")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(e) = e.as_str() {
                    names.insert(e.to_owned());
                }
            }
        }
    }
    Ok(names.into_iter().collect())
}

#[derive(sqlx::FromRow)]
struct TenantRowDb {
    key: String,
    kind: String,
    item: Option<Value>,
    base_hash: Option<String>,
    content_hash: Option<String>,
    updated_by: String,
    updated_by_name: String,
    updated_at: DateTime<Utc>,
}

/// The tenant's rows of `object` (fixture `TenantRow` with `updated_by_name`), oldest write first.
async fn tenant_rows(c: &mut PgConnection, job: Uuid, object: &str) -> ApiResult<Vec<Value>> {
    let rows = sqlx::query_as::<_, TenantRowDb>(
        "SELECT r.key, r.kind, r.item, r.base_hash, r.content_hash, r.updated_by, \
                COALESCE(NULLIF(btrim(u.display_name), ''), NULLIF(btrim(u.email), ''), r.updated_by) \
                    AS updated_by_name, r.updated_at \
           FROM job_overlay_row r LEFT JOIN users u ON u.sub = r.updated_by \
          WHERE r.job_id = $1 AND r.object = $2 \
          ORDER BY r.updated_at, r.key",
    )
    .bind(job)
    .bind(object)
    .fetch_all(&mut *c)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            json!({
                "key": r.key,
                "kind": r.kind,
                "item": r.item,
                "base_hash": r.base_hash,
                "content_hash": r.content_hash,
                "updated_by": r.updated_by,
                "updated_by_name": r.updated_by_name,
                "updated_at": iso(r.updated_at),
            })
        })
        .collect())
}

/// `GET /api/jobs/{job}/{object}` (fixture `refinementLayers`, `OverlayLayers`): both sides of one refinable object.
/// 404 `unknown_object` for anything but the five overlay objects.
pub async fn layers(
    c: &mut PgConnection,
    reg: &Registry,
    compiler: &dyn TemplateCompiler,
    job: &JobRef,
    object: &str,
) -> ApiResult<Value> {
    if !OVERLAY_OBJECTS.contains(&object) {
        return Err(unknown_object(object));
    }
    let bundle = draft_bundle(&mut *c, compiler, job.id).await?;
    let base: Vec<Value> = bundle
        .as_ref()
        .and_then(|b| b.objects.get(object))
        .unwrap_or_default()
        .iter()
        .map(|i| Value::Object(i.clone()))
        .collect();
    let tenant = tenant_rows(&mut *c, job.id, object).await?;
    let draft_rev = design::draft_rev(&mut *c, job).await?;
    let entities = entities(&mut *c, reg, job, bundle.as_ref()).await?;
    Ok(json!({
        "object": object,
        "base": base,
        "tenant": tenant,
        "draft_rev": draft_rev,
        "entities": entities,
    }))
}

// ---- the writes (run inside `design::track`'s mutate) -----------------------------------------------------------

/// Where one refinement row lives, and who writes it.
#[derive(Clone)]
struct RowAt {
    job: Uuid,
    object: String,
    key: String,
    by: String,
}

/// What a write does to the tenant's overlay row.
enum RowChange {
    /// Hide the base item.
    Tombstone { base_hash: String },
    /// Drop the tenant's row (unhide, revert).
    Remove,
    /// Keep the row on the new base.
    Rebase {
        kind: String,
        base_hash: Option<String>,
    },
    /// New content for the existing row (its kind and `base_hash` are kept).
    Content { item: Value, content_hash: String },
    /// Copy-on-write a base item into a new override row.
    Override {
        item: Value,
        base_hash: String,
        content_hash: String,
    },
    /// A new row with no base (an instruction slot the release does not have).
    Extension { item: Value, content_hash: String },
}

/// A choice saved on the place's open conflicts (fixture `resolveConflicts`).
struct Decision {
    choice: &'static str,
    draft_key: String,
    at: String,
}

async fn decide(c: &mut PgConnection, at: &RowAt, d: &Decision) -> ApiResult<()> {
    sqlx::query(
        "UPDATE release_conflict \
            SET decision = jsonb_build_object('choice', $4::text, 'draft_key', $5::text, 'draft_id', NULL::text, \
                                              'by', $6::text, 'at', $7::text) \
          WHERE job_id = $1 AND object = $2 AND key = $3 AND status = 'open'",
    )
    .bind(at.job)
    .bind(&at.object)
    .bind(&at.key)
    .bind(d.choice)
    .bind(&d.draft_key)
    .bind(&at.by)
    .bind(&d.at)
    .execute(&mut *c)
    .await?;
    Ok(())
}

/// Fixture `linkDecisions`: point each open decision of the place at the ledger row that now carries it.
async fn link_decisions(
    c: &mut PgConnection,
    job: Uuid,
    object: &str,
    key: &str,
    draft_id: Option<String>,
) -> ApiResult<()> {
    sqlx::query(
        "UPDATE release_conflict SET decision = jsonb_set(decision, '{draft_id}', $4) \
          WHERE job_id = $1 AND object = $2 AND key = $3 AND status = 'open' AND decision IS NOT NULL",
    )
    .bind(job)
    .bind(object)
    .bind(key)
    .bind(json!(draft_id))
    .execute(&mut *c)
    .await?;
    Ok(())
}

async fn write_overlay(
    c: &mut PgConnection,
    at: RowAt,
    change: RowChange,
    decision: Option<Decision>,
) -> ApiResult<()> {
    match change {
        RowChange::Tombstone { base_hash } => {
            sqlx::query(
                "INSERT INTO job_overlay_row \
                        (job_id, object, key, kind, item, base_hash, content_hash, updated_by) \
                 VALUES ($1, $2, $3, 'tombstone', NULL, $4, $5, $6) \
                 ON CONFLICT (tenant_id, job_id, object, key) DO UPDATE \
                    SET kind = EXCLUDED.kind, item = NULL, base_hash = EXCLUDED.base_hash, \
                        content_hash = EXCLUDED.content_hash, updated_by = EXCLUDED.updated_by, \
                        updated_at = clock_timestamp()",
            )
            .bind(at.job)
            .bind(&at.object)
            .bind(&at.key)
            .bind(&base_hash)
            .bind(format!("tomb-{base_hash}"))
            .bind(&at.by)
            .execute(&mut *c)
            .await?;
        }
        RowChange::Remove => {
            sqlx::query(
                "DELETE FROM job_overlay_row WHERE job_id = $1 AND object = $2 AND key = $3",
            )
            .bind(at.job)
            .bind(&at.object)
            .bind(&at.key)
            .execute(&mut *c)
            .await?;
        }
        RowChange::Rebase { kind, base_hash } => {
            sqlx::query(
                "UPDATE job_overlay_row \
                    SET kind = $4, base_hash = $5, updated_by = $6, updated_at = clock_timestamp() \
                  WHERE job_id = $1 AND object = $2 AND key = $3",
            )
            .bind(at.job)
            .bind(&at.object)
            .bind(&at.key)
            .bind(&kind)
            .bind(&base_hash)
            .bind(&at.by)
            .execute(&mut *c)
            .await?;
        }
        RowChange::Content { item, content_hash } => {
            sqlx::query(
                "UPDATE job_overlay_row \
                    SET item = $4, content_hash = $5, updated_by = $6, updated_at = clock_timestamp() \
                  WHERE job_id = $1 AND object = $2 AND key = $3",
            )
            .bind(at.job)
            .bind(&at.object)
            .bind(&at.key)
            .bind(&item)
            .bind(&content_hash)
            .bind(&at.by)
            .execute(&mut *c)
            .await?;
        }
        RowChange::Override {
            item,
            base_hash,
            content_hash,
        } => {
            sqlx::query(
                "INSERT INTO job_overlay_row \
                        (job_id, object, key, kind, item, base_hash, content_hash, updated_by) \
                 VALUES ($1, $2, $3, 'override', $4, $5, $6, $7)",
            )
            .bind(at.job)
            .bind(&at.object)
            .bind(&at.key)
            .bind(&item)
            .bind(&base_hash)
            .bind(&content_hash)
            .bind(&at.by)
            .execute(&mut *c)
            .await?;
        }
        RowChange::Extension { item, content_hash } => {
            sqlx::query(
                "INSERT INTO job_overlay_row \
                        (job_id, object, key, kind, item, base_hash, content_hash, updated_by) \
                 VALUES ($1, $2, $3, 'extension', $4, NULL, $5, $6)",
            )
            .bind(at.job)
            .bind(&at.object)
            .bind(&at.key)
            .bind(&item)
            .bind(&content_hash)
            .bind(&at.by)
            .execute(&mut *c)
            .await?;
        }
    }
    if let Some(d) = decision {
        decide(&mut *c, &at, &d).await?;
    }
    Ok(())
}

/// What a write does to the job's setting override.
enum SettingChange {
    /// `value = NULL` (hide the base value).
    Hide { base_hash: Option<String> },
    /// Drop the override (unhide, revert).
    Remove,
    /// Keep the value, re-stamped onto the new base (rebase, "Keep mine"): `base_hash` = the hash of the current
    /// base value (`None` when the draft bundle has no base for the key). The setting target compares
    /// `(value, base_hash)`, so this is a recorded draft change that carries the conflict decision.
    Rebase { base_hash: Option<String> },
}

async fn write_setting(
    c: &mut PgConnection,
    at: RowAt,
    change: SettingChange,
    decision: Option<Decision>,
) -> ApiResult<()> {
    match change {
        SettingChange::Hide { base_hash } => {
            sqlx::query(
                "INSERT INTO job_setting_override (job_id, key, value, base_hash, updated_by) \
                 VALUES ($1, $2, NULL, $3, $4) \
                 ON CONFLICT (tenant_id, job_id, key) DO UPDATE \
                    SET value = NULL, base_hash = EXCLUDED.base_hash, updated_by = EXCLUDED.updated_by, \
                        updated_at = clock_timestamp()",
            )
            .bind(at.job)
            .bind(&at.key)
            .bind(&base_hash)
            .bind(&at.by)
            .execute(&mut *c)
            .await?;
        }
        SettingChange::Remove => {
            sqlx::query("DELETE FROM job_setting_override WHERE job_id = $1 AND key = $2")
                .bind(at.job)
                .bind(&at.key)
                .execute(&mut *c)
                .await?;
        }
        SettingChange::Rebase { base_hash } => {
            sqlx::query(
                "UPDATE job_setting_override \
                    SET base_hash = $3, updated_by = $4, updated_at = clock_timestamp() \
                  WHERE job_id = $1 AND key = $2",
            )
            .bind(at.job)
            .bind(&at.key)
            .bind(&base_hash)
            .bind(&at.by)
            .execute(&mut *c)
            .await?;
        }
    }
    if let Some(d) = decision {
        decide(&mut *c, &at, &d).await?;
    }
    Ok(())
}

// ---- the J4 journey writes (B8 T7, Q-R3 (a)) -----------------------------------------------------------------------

/// What a J4 journey write does to a job's `activity` row (`job_overlay_row`, object [`ACTIVITY`]).
#[derive(Debug)]
pub(crate) enum ActivityWrite {
    /// Drop the tenant's row (restore, reset, remove an extension).
    Remove,
    /// Hide the base activity: a tombstone over a base whose [`base_hash`] is given.
    Tombstone { base_hash: String },
    /// A sparse override of a base activity, `{key, label?, card?}`, over the base with `base_hash`.
    Override { item: Value, base_hash: String },
    /// The tenant's own activity (the whole [`crate::journey::model::JourneyActivity`] item, plus `card?`).
    Extension { item: Value },
}

/// One tracked J4 write of the `activity` row `key` (B8 T7): ledger kind `activity`, area `journey`, page J4,
/// ledger key `journey:<key>`. Wraps [`write_overlay`], so the hashes (`base_hash` of the base item, `content_hash`
/// of the tenant's item, `tomb-<base_hash>`) and the conflict-decision semantics are J13's: `choice` (`reverted`)
/// is saved on every open conflict of the place, and after the tracked write each open decision points at the
/// ledger row that now carries it (`draft_key` = `journey:<key>`). A row of another kind than the write's (an
/// extension over a tombstone …) is replaced. The caller has validated; nothing here refuses. Runs in the caller's
/// job-scoped transaction. Returns the job's `draft_rev` after the write.
pub(crate) async fn write_activity_row(
    c: &mut PgConnection,
    reg: &Registry,
    j: &JobCtx,
    key: &str,
    label: String,
    w: ActivityWrite,
    choice: Option<&'static str>,
) -> ApiResult<i64> {
    let at = RowAt {
        job: j.job_id,
        object: ACTIVITY.to_owned(),
        key: key.to_owned(),
        by: j.t.caller.sub.clone(),
    };
    let decision = choice.map(|choice| Decision {
        choice,
        draft_key: format!("journey:{key}"),
        at: now_iso(),
    });
    let dw = DraftWrite {
        area: "journey",
        page: "J4",
        kind: "activity",
        r#ref: place_ref(&[("key", key)]),
        label: Label::Now(label),
    };
    let out = design::track::<(), _>(&mut *c, reg, j, dw, move |c| {
        Box::pin(async move {
            let current: Option<String> = sqlx::query_scalar(
                "SELECT kind FROM job_overlay_row WHERE job_id = $1 AND object = $2 AND key = $3",
            )
            .bind(at.job)
            .bind(&at.object)
            .bind(&at.key)
            .fetch_optional(&mut *c)
            .await?;
            // a row of another kind than an override / extension write is replaced: drop it, then write afresh
            let wanted = match &w {
                ActivityWrite::Override { .. } => Some("override"),
                ActivityWrite::Extension { .. } => Some("extension"),
                ActivityWrite::Remove | ActivityWrite::Tombstone { .. } => None,
            };
            if let (Some(have), Some(want)) = (current.as_deref(), wanted)
                && have != want
            {
                write_overlay(&mut *c, at.clone(), RowChange::Remove, None).await?;
            }
            let change = match w {
                ActivityWrite::Remove => RowChange::Remove,
                ActivityWrite::Tombstone { base_hash } => RowChange::Tombstone { base_hash },
                ActivityWrite::Override { item, base_hash } => {
                    let content_hash = item_hash(item.as_object().ok_or_else(|| {
                        internal("an activity override item is not an object".into())
                    })?)?;
                    if current.as_deref() == Some("override") {
                        RowChange::Content { item, content_hash }
                    } else {
                        RowChange::Override {
                            item,
                            base_hash,
                            content_hash,
                        }
                    }
                }
                ActivityWrite::Extension { item } => {
                    let content_hash = item_hash(item.as_object().ok_or_else(|| {
                        internal("an activity extension item is not an object".into())
                    })?)?;
                    if current.as_deref() == Some("extension") {
                        RowChange::Content { item, content_hash }
                    } else {
                        RowChange::Extension { item, content_hash }
                    }
                }
            };
            write_overlay(&mut *c, at, change, decision).await
        })
    })
    .await?;
    let draft_id = out.change.as_ref().map(|r| format!("d-{}", r.id));
    link_decisions(&mut *c, j.job_id, ACTIVITY, key, draft_id).await?;
    Ok(out.draft_rev)
}

// ---- refine ---------------------------------------------------------------------------------------------------------

/// 409 `edit_on_journey_page`: the object `activity` is refined on the job's Journey page (J4, kind `activity`),
/// never through J13's `refine` or `resolveConflict` (Q-R3 (a)).
pub(crate) fn edit_on_journey_page(object: &str) -> ApiResult<()> {
    if object == ACTIVITY {
        return Err(ApiError::Conflict(
            "edit_on_journey_page",
            "Activities are refined on the job's Journey page, not here.".into(),
        ));
    }
    Ok(())
}

/// `POST /api/jobs/{job}/refinements/{op}` `{object, key}` (fixture `refineIn` / `refineRaw`), in the caller's
/// job-scoped transaction: one tracked write, kind `overlay` (or `setting` for `job_setting`), page J13. Returns the
/// job's `draft_rev` after the write. Refusals (nothing is written):
/// - 404 `unknown_object`;
/// - 409 `edit_on_journey_page` (object `activity`, Q-R3 (a): the Journey page writes it);
/// - 409 `not_in_release`, `already_hidden`, `not_hidden`, `nothing_to_revert`, `nothing_to_rebase`,
///   `parent_missing`;
/// - 422 `required_activity` (a base item with `required: true`), `unknown_setting` (a `job_setting` key that is not
///   `actor` or `stance` — KH T4 retired `preamble`, D108), `invalid_refinement` (a `knowledge_pack` row that would not be a tombstone),
///   `instruction_not_refinable` (an `instruction` key other than `tone`, `escalation`, `format`; D108).
pub async fn refine(
    c: &mut PgConnection,
    reg: &Registry,
    compiler: &dyn TemplateCompiler,
    j: &JobCtx,
    op: Op,
    object: &str,
    key: &str,
) -> ApiResult<i64> {
    if object == JOB_SETTING {
        return refine_setting(c, reg, compiler, j, op, key).await;
    }
    if !OVERLAY_OBJECTS.contains(&object) {
        return Err(unknown_object(object));
    }
    edit_on_journey_page(object)?;
    check_refinable(object, key)?;
    lock_job(&mut *c, j.job_id).await?;
    let bundle = draft_bundle(&mut *c, compiler, j.job_id).await?;
    let base: Option<OverlayItem> = bundle
        .as_ref()
        .and_then(|b| base_item(b, object, key))
        .cloned();
    let current: Option<String> = sqlx::query_scalar(
        "SELECT kind FROM job_overlay_row WHERE job_id = $1 AND object = $2 AND key = $3",
    )
    .bind(j.job_id)
    .bind(object)
    .bind(key)
    .fetch_optional(&mut *c)
    .await?;
    let (change, choice): (RowChange, Option<&'static str>) = match op {
        Op::Hide => {
            let Some(base) = &base else {
                return Err(ApiError::Conflict(
                    "not_in_release",
                    format!("“{key}” is not in the release; remove your own row with Revert."),
                ));
            };
            if current.as_deref() == Some(TOMBSTONE) {
                return Err(ApiError::Conflict(
                    "already_hidden",
                    format!("“{key}” is already hidden."),
                ));
            }
            if base.get("required") == Some(&Value::Bool(true)) {
                return Err(ApiError::Unprocessable(
                    "required_activity",
                    format!("“{key}” is required by the job template and cannot be hidden."),
                ));
            }
            (
                RowChange::Tombstone {
                    base_hash: base_hash(object, base)?,
                },
                None,
            )
        }
        Op::Unhide => {
            if current.as_deref() != Some(TOMBSTONE) {
                return Err(ApiError::Conflict(
                    "not_hidden",
                    format!("“{key}” is not hidden."),
                ));
            }
            (RowChange::Remove, Some("reverted"))
        }
        Op::Revert => {
            if current.is_none() {
                return Err(ApiError::Conflict(
                    "nothing_to_revert",
                    format!("“{key}” has no refinement of yours."),
                ));
            }
            (RowChange::Remove, Some("reverted"))
        }
        Op::Rebase => {
            let Some(kind) = current.as_deref() else {
                return Err(ApiError::Conflict(
                    "nothing_to_rebase",
                    format!("“{key}” has no refinement of yours."),
                ));
            };
            let orphan: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM release_conflict \
                                 WHERE job_id = $1 AND object = $2 AND key = $3 \
                                   AND status = 'open' AND kind = 'parent_missing')",
            )
            .bind(j.job_id)
            .bind(object)
            .bind(key)
            .fetch_one(&mut *c)
            .await?;
            if orphan {
                return Err(ApiError::Conflict(
                    "parent_missing",
                    "Its parent is gone from the release; restore the parent or revert.".into(),
                ));
            }
            let change = match &base {
                Some(b) => RowChange::Rebase {
                    kind: kind.to_owned(),
                    base_hash: Some(base_hash(object, b)?),
                },
                None if kind == TOMBSTONE => {
                    return Err(ApiError::Conflict(
                        "nothing_to_rebase",
                        format!(
                            "The release removed “{key}”; there is nothing to hide. Revert instead."
                        ),
                    ));
                }
                None => RowChange::Rebase {
                    kind: "extension".to_owned(),
                    base_hash: None,
                },
            };
            (change, Some("rebased"))
        }
    };
    if object == KNOWLEDGE_PACK && !tombstone_only(&change) {
        return Err(invalid_refinement(key));
    }
    let at = RowAt {
        job: j.job_id,
        object: object.to_owned(),
        key: key.to_owned(),
        by: j.t.caller.sub.clone(),
    };
    let decision = choice.map(|choice| Decision {
        choice,
        draft_key: format!("overlay:{object}:{key}"),
        at: now_iso(),
    });
    // PK-2: the kinds' template pins follow the tombstones, in the same tracked write
    let refresh = (object == KNOWLEDGE_PACK).then(|| (job_ref(j), key.to_owned()));
    let w = DraftWrite {
        area: "refinements",
        page: "J13",
        kind: "overlay",
        r#ref: place_ref(&[("object", object), ("key", key)]),
        label: Label::Now(format!("{} {key} {}", object.replace('_', " "), op.done())),
    };
    let out = design::track::<(), _>(&mut *c, reg, j, w, move |c| {
        Box::pin(async move {
            write_overlay(&mut *c, at, change, decision).await?;
            if let Some((job, key)) = refresh {
                kinds::refresh_template_packs(c, &job, &key).await?;
            }
            Ok::<(), ApiError>(())
        })
    })
    .await?;
    let draft_id = out.change.as_ref().map(|r| format!("d-{}", r.id));
    link_decisions(&mut *c, j.job_id, object, key, draft_id).await?;
    Ok(out.draft_rev)
}

/// The hash of setting `key`'s base value in the job's DRAFT bundle; `None` without a base.
async fn base_setting_hash(
    c: &mut PgConnection,
    compiler: &dyn TemplateCompiler,
    job: Uuid,
    key: &str,
) -> ApiResult<Option<String>> {
    let bundle = draft_bundle(&mut *c, compiler, job).await?;
    bundle
        .as_ref()
        .and_then(|b| b.settings.get(key))
        .map(setting_hash)
        .transpose()
}

/// `refine` of a job setting (fixture `refineRaw` for `job_setting`): kind `setting`, area `settings`, page J13.
async fn refine_setting(
    c: &mut PgConnection,
    reg: &Registry,
    compiler: &dyn TemplateCompiler,
    j: &JobCtx,
    op: Op,
    key: &str,
) -> ApiResult<i64> {
    if !SETTING_KEYS.contains(&key) {
        return Err(ApiError::Unprocessable(
            "unknown_setting",
            format!("No setting “{key}”."),
        ));
    }
    lock_job(&mut *c, j.job_id).await?;
    let current: Option<Option<String>> =
        sqlx::query_scalar("SELECT value FROM job_setting_override WHERE job_id = $1 AND key = $2")
            .bind(j.job_id)
            .bind(key)
            .fetch_optional(&mut *c)
            .await?;
    let (change, choice): (SettingChange, Option<&'static str>) = match op {
        Op::Revert => {
            if current.is_none() {
                return Err(ApiError::Conflict(
                    "nothing_to_revert",
                    format!("Setting {key} uses the release's value."),
                ));
            }
            (SettingChange::Remove, Some("reverted"))
        }
        Op::Rebase => {
            if current.is_none() {
                return Err(ApiError::Conflict(
                    "nothing_to_rebase",
                    format!("Setting {key} has no override."),
                ));
            }
            let base_hash = base_setting_hash(&mut *c, compiler, j.job_id, key).await?;
            (SettingChange::Rebase { base_hash }, Some("rebased"))
        }
        Op::Hide => {
            let base_hash = base_setting_hash(&mut *c, compiler, j.job_id, key).await?;
            (SettingChange::Hide { base_hash }, None)
        }
        Op::Unhide => {
            if !matches!(current, Some(None)) {
                return Err(ApiError::Conflict(
                    "not_hidden",
                    format!("Setting {key} is not hidden."),
                ));
            }
            (SettingChange::Remove, None)
        }
    };
    let at = RowAt {
        job: j.job_id,
        object: JOB_SETTING.to_owned(),
        key: key.to_owned(),
        by: j.t.caller.sub.clone(),
    };
    let decision = choice.map(|choice| Decision {
        choice,
        draft_key: format!("setting:{key}"),
        at: now_iso(),
    });
    let w = DraftWrite {
        area: "settings",
        page: "J13",
        kind: "setting",
        r#ref: place_ref(&[("key", key)]),
        label: Label::Now(format!("Setting {key} {}", op.done())),
    };
    let out = design::track::<(), _>(&mut *c, reg, j, w, move |c| {
        Box::pin(write_setting(c, at, change, decision))
    })
    .await?;
    let draft_id = out.change.as_ref().map(|r| format!("d-{}", r.id));
    link_decisions(&mut *c, j.job_id, JOB_SETTING, key, draft_id).await?;
    Ok(out.draft_rev)
}

// ---- saveContextSpaceOverride ---------------------------------------------------------------------------------------

/// `PUT /api/jobs/{job}/context-spaces/{key}` (fixture `saveContextSpaceOverride`), in the caller's job-scoped
/// transaction: copy-on-write the base context space into an override row (`base_hash` = the base item's hash), or
/// update the tenant's existing row (its kind and `base_hash` are kept). One tracked write, kind `overlay`, area
/// `context spaces`, page J12. Returns the job's `draft_rev`.
///
/// B5 T7 (J12 `saveContextSpaceRequired`, plan R12): a patch of `required` ONLY is recorded under B5's projection
/// kind `space_required` (ref `{key}`, label `<label>: required fields <slots | none>`), with the fixture's slot
/// rules ([`j12_slots`]); `required: null` drops `required` from the tenant's item (= the template's slots), and
/// removes an override that is then the base again (no row at all is a no-op). Any other patch keeps B2's `overlay`
/// kind and rules.
///
/// Refusals (nothing is written):
/// - 422 `invalid_body` (nothing to change; blank or over-long label; a required field that is not snake_case, in a
///   patch that also changes the label or entities);
/// - 422 `invalid_slot` / `duplicate_slot` (a required-only patch);
/// - 422 `unknown_entity` (an entity not in [`entities`]);
/// - 404 `context_space_not_found` (neither a base nor a tenant row);
/// - 409 `hidden` (the tenant hid it).
pub async fn save_context_space(
    c: &mut PgConnection,
    reg: &Registry,
    compiler: &dyn TemplateCompiler,
    j: &JobCtx,
    key: &str,
    mut p: ContextSpacePatch,
) -> ApiResult<i64> {
    let required_only = p.required_only();
    if required_only && let Some(Some(r)) = &p.required {
        p.required = Some(Some(j12_slots(r)?));
    }
    if p.label.is_none() && p.required.is_none() && p.entities.is_none() {
        return Err(ApiError::Unprocessable(
            "invalid_body",
            "Nothing to change.".into(),
        ));
    }
    if let Some(l) = &p.label {
        if l.trim().is_empty() {
            return Err(ApiError::Unprocessable(
                "invalid_body",
                "Label is required.".into(),
            ));
        }
        // UTF-16 units, as the fixture's `.length` and T7's setting length
        if l.encode_utf16().count() > MAX_LABEL_CHARS {
            return Err(ApiError::Unprocessable(
                "invalid_body",
                format!("Label is at most {MAX_LABEL_CHARS} characters."),
            ));
        }
    }
    let bad: Vec<&str> = p
        .required
        .iter()
        .flatten()
        .flatten()
        .filter(|r| !is_slot(r))
        .map(String::as_str)
        .collect();
    if !bad.is_empty() {
        return Err(ApiError::Unprocessable(
            "invalid_body",
            format!(
                "Required fields are snake_case names (not {}).",
                bad.join(", ")
            ),
        ));
    }
    lock_job(&mut *c, j.job_id).await?;
    let bundle = draft_bundle(&mut *c, compiler, j.job_id).await?;
    if let Some(wanted) = &p.entities {
        let known = entities(&mut *c, reg, &job_ref(j), bundle.as_ref()).await?;
        let unknown: Vec<&str> = wanted
            .iter()
            .filter(|e| !known.contains(*e))
            .map(String::as_str)
            .collect();
        if !unknown.is_empty() {
            return Err(ApiError::Unprocessable(
                "unknown_entity",
                format!("Unknown entity {}.", unknown.join(", ")),
            ));
        }
    }
    let base: Option<OverlayItem> = bundle
        .as_ref()
        .and_then(|b| base_item(b, CONTEXT_SPACE, key))
        .cloned();
    let current: Option<(String, Option<Value>)> = sqlx::query_as(
        "SELECT kind, item FROM job_overlay_row WHERE job_id = $1 AND object = $2 AND key = $3",
    )
    .bind(j.job_id)
    .bind(CONTEXT_SPACE)
    .bind(key)
    .fetch_optional(&mut *c)
    .await?;
    if base.is_none() && current.is_none() {
        return Err(ApiError::Missing(
            "context_space_not_found",
            format!("No context space “{key}”."),
        ));
    }
    if current.as_ref().is_some_and(|(kind, _)| kind == TOMBSTONE) {
        return Err(ApiError::Conflict(
            "hidden",
            format!("“{key}” is hidden; unhide it first."),
        ));
    }
    let mine: Option<Map<String, Value>> = current
        .as_ref()
        .and_then(|(_, item)| item.as_ref())
        .and_then(Value::as_object)
        .cloned();
    let mut item: Map<String, Value> = mine.or_else(|| base.clone()).unwrap_or_default();
    if let Some(l) = p.label {
        item.insert("label".to_owned(), Value::String(l));
    }
    let null_required = matches!(p.required, Some(None));
    match p.required {
        Some(Some(r)) => {
            item.insert("required".to_owned(), json!(r));
        }
        Some(None) => {
            item.remove("required");
        }
        None => {}
    }
    if let Some(e) = p.entities {
        item.insert("entities".to_owned(), json!(e));
    }
    item.insert("key".to_owned(), Value::String(key.to_owned()));
    let content_hash = item_hash(&item)?;
    // the ledger label of a J12 write: the space's label and the slots it runs after the write
    let after_label: Option<String> = item.get("label").and_then(Value::as_str).map(str::to_owned);
    let after_required: Vec<String> = item
        .get("required")
        .or_else(|| base.as_ref().and_then(|b| b.get("required")))
        .and_then(Value::as_array)
        .map(|xs| {
            xs.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    // `required: null` on an override that is then the base again (or on no row at all): the row goes
    let back_to_base = null_required
        && match (&current, &base) {
            (None, _) => true,
            (Some((kind, _)), Some(b)) if kind == "override" => {
                let mut b = b.clone();
                b.remove("required");
                b == item
            }
            _ => false,
        };
    let change = match (&current, &base) {
        _ if back_to_base => RowChange::Remove,
        (Some(_), _) => RowChange::Content {
            item: Value::Object(item),
            content_hash,
        },
        (None, Some(b)) => RowChange::Override {
            item: Value::Object(item),
            base_hash: item_hash(b)?,
            content_hash,
        },
        (None, None) => {
            return Err(internal(
                "a context space with neither a base nor a tenant row passed the 404".into(),
            ));
        }
    };
    let at = RowAt {
        job: j.job_id,
        object: CONTEXT_SPACE.to_owned(),
        key: key.to_owned(),
        by: j.t.caller.sub.clone(),
    };
    let w = if required_only {
        let label = after_label
            .filter(|l| !l.trim().is_empty())
            .unwrap_or_else(|| key.to_owned());
        let slots = if after_required.is_empty() {
            "none".to_owned()
        } else {
            after_required.join(", ")
        };
        DraftWrite {
            area: "context spaces",
            page: "J12",
            kind: "space_required",
            r#ref: place_ref(&[("key", key)]),
            label: Label::Now(format!("{label}: required fields {slots}")),
        }
    } else {
        DraftWrite {
            area: "context spaces",
            page: "J12",
            kind: "overlay",
            r#ref: place_ref(&[("object", CONTEXT_SPACE), ("key", key)]),
            label: Label::Now(format!("Context space {key} changed")),
        }
    };
    let out = design::track::<(), _>(&mut *c, reg, j, w, move |c| {
        Box::pin(write_overlay(c, at, change, None))
    })
    .await?;
    Ok(out.draft_rev)
}

// ---- instructions (D108) ---------------------------------------------------------------------------------------

/// `PUT /api/jobs/{job}/instructions/{slot}` `{text, language?}`.
#[derive(Debug, Default, Deserialize)]
pub struct InstructionPatch {
    pub text: Option<String>,
    pub language: Option<String>,
}

/// `PUT /api/jobs/{job}/instructions/{slot}` (KH T4, D108, D8), in the caller's job-scoped transaction: the tenant's
/// own text for a refinable slot, written as an override row of object `instruction` (`base_hash` = the hash of the
/// release's folded slot; `item` = `{key, text, language?}`), or an extension when the draft bundle has no such slot;
/// a row the tenant already has is updated (its kind and `base_hash` kept). One tracked write, kind `overlay`, area
/// `instructions`, page J13. Returns the job's `draft_rev`. Refusals (nothing is written):
/// - 422 `instruction_not_refinable` (a slot other than `tone`, `escalation`, `format`);
/// - 422 `invalid_body` (blank or over-long text, a language that is not a tag, a line that starts with a prompt
///   section marker — `[layer `, `[task `, `[passages]`: crate::runtime::prompt::forges_a_section, every Unicode line separator a line break);
/// - 409 `hidden` (the tenant hid the slot; unhide it first).
pub async fn save_instruction(
    c: &mut PgConnection,
    reg: &Registry,
    compiler: &dyn TemplateCompiler,
    j: &JobCtx,
    slot: &str,
    p: InstructionPatch,
) -> ApiResult<i64> {
    check_refinable(INSTRUCTION, slot)?;
    let text = p.text.unwrap_or_default().trim().to_owned();
    if text.encode_utf16().count() > MAX_INSTRUCTION_CHARS {
        return Err(ApiError::Unprocessable(
            "invalid_body",
            format!("An instruction is at most {MAX_INSTRUCTION_CHARS} characters."),
        ));
    }
    if crate::runtime::prompt::forges_a_section(&text) {
        return Err(ApiError::Unprocessable(
            "invalid_body",
            "An instruction line cannot start with a prompt section marker ([layer …], [task …] or [passages])."
                .into(),
        ));
    }
    let mut item = Map::new();
    item.insert("text".to_owned(), Value::String(text));
    if let Some(l) = p
        .language
        .map(|l| l.trim().to_owned())
        .filter(|l| !l.is_empty())
    {
        item.insert("language".to_owned(), Value::String(l));
    }
    // the kind's own field rules (non-blank text, a language tag)
    if let Err(errors) =
        model_kinds::validate_row(Kind::Instruction, RowOp::Add, &Value::Object(item.clone()))
    {
        let first = errors.first().map(ToString::to_string).unwrap_or_default();
        return Err(ApiError::Unprocessable(
            "invalid_body",
            format!("The instruction is not valid: {first}"),
        ));
    }
    item.insert("key".to_owned(), Value::String(slot.to_owned()));
    lock_job(&mut *c, j.job_id).await?;
    let bundle = draft_bundle(&mut *c, compiler, j.job_id).await?;
    let base: Option<OverlayItem> = bundle
        .as_ref()
        .and_then(|b| base_item(b, INSTRUCTION, slot))
        .cloned();
    let current: Option<String> = sqlx::query_scalar(
        "SELECT kind FROM job_overlay_row WHERE job_id = $1 AND object = $2 AND key = $3",
    )
    .bind(j.job_id)
    .bind(INSTRUCTION)
    .bind(slot)
    .fetch_optional(&mut *c)
    .await?;
    if current.as_deref() == Some(TOMBSTONE) {
        return Err(ApiError::Conflict(
            "hidden",
            format!("The instruction “{slot}” is hidden; unhide it first."),
        ));
    }
    let content_hash = item_hash(&item)?;
    let change = match (&current, &base) {
        (Some(_), _) => RowChange::Content {
            item: Value::Object(item),
            content_hash,
        },
        (None, Some(b)) => RowChange::Override {
            item: Value::Object(item),
            base_hash: base_hash(INSTRUCTION, b)?,
            content_hash,
        },
        (None, None) => RowChange::Extension {
            item: Value::Object(item),
            content_hash,
        },
    };
    let at = RowAt {
        job: j.job_id,
        object: INSTRUCTION.to_owned(),
        key: slot.to_owned(),
        by: j.t.caller.sub.clone(),
    };
    let w = DraftWrite {
        area: "instructions",
        page: "J13",
        kind: "overlay",
        r#ref: place_ref(&[("object", INSTRUCTION), ("key", slot)]),
        label: Label::Now(format!("Instruction {slot} changed")),
    };
    let out = design::track::<(), _>(&mut *c, reg, j, w, move |c| {
        Box::pin(write_overlay(c, at, change, None))
    })
    .await?;
    Ok(out.draft_rev)
}

// ---- B5 refineGateOption (generated options) -------------------------------------------------------------------------

/// B5 `refineGateOption` of a GENERATED gate option (`PUT /api/jobs/{job}/gates/{gate}/options/{option}`, B5 plan R2
/// and T8), in the caller's job-scoped transaction: hide it (a `tombstone` row), show it again, or relabel /
/// redescribe it (copy-on-write the base item into an `override` row, or update the tenant's row). Generated
/// options (GT/GS/GA/GO) are the draft bundle's `objects.gate_option`, key `<GATE> · <option>`; a LIBRARY option's
/// refinement is B5's own `gate` kind, not this. One tracked write, kind `overlay`, area `gates`, page J7. A patch
/// that leaves the item equal to the base removes the tenant's row. Returns the job's `draft_rev`. Refusals (nothing
/// is written):
/// - 422 `invalid_body` (nothing to change; blank or over-long label);
/// - 404 `option_not_found` (no generated option `key` in the draft bundle; the tenant's leftover row of a removed
///   option can only be dropped, with `{hidden: false}`).
pub async fn save_gate_option(
    c: &mut PgConnection,
    reg: &Registry,
    compiler: &dyn TemplateCompiler,
    j: &JobCtx,
    key: &str,
    p: GateOptionPatch,
) -> ApiResult<i64> {
    if p.label.is_none() && p.description.is_none() && p.hidden.is_none() {
        return Err(ApiError::Unprocessable(
            "invalid_body",
            "Nothing to change.".into(),
        ));
    }
    if let Some(l) = &p.label {
        if l.trim().is_empty() {
            return Err(ApiError::Unprocessable(
                "invalid_body",
                "Label is required.".into(),
            ));
        }
        if l.encode_utf16().count() > MAX_LABEL_CHARS {
            return Err(ApiError::Unprocessable(
                "invalid_body",
                format!("Label is at most {MAX_LABEL_CHARS} characters."),
            ));
        }
    }
    lock_job(&mut *c, j.job_id).await?;
    let bundle = draft_bundle(&mut *c, compiler, j.job_id).await?;
    let base: Option<OverlayItem> = bundle
        .as_ref()
        .and_then(|b| base_item(b, GATE_OPTION, key))
        .cloned();
    let current: Option<(String, Option<Value>)> = sqlx::query_as(
        "SELECT kind, item FROM job_overlay_row WHERE job_id = $1 AND object = $2 AND key = $3",
    )
    .bind(j.job_id)
    .bind(GATE_OPTION)
    .bind(key)
    .fetch_optional(&mut *c)
    .await?;
    let not_found =
        || ApiError::Missing("option_not_found", format!("No generated option “{key}”."));
    let hidden_now = current.as_ref().is_some_and(|(kind, _)| kind == TOMBSTONE);
    let (changes, what): (Vec<RowChange>, &str) = match &base {
        None => {
            let drop_only = p.label.is_none() && p.description.is_none() && p.hidden == Some(false);
            if current.is_none() || !drop_only {
                return Err(not_found());
            }
            (vec![RowChange::Remove], "back to the release value")
        }
        Some(b) if p.hidden.unwrap_or(hidden_now) => (
            vec![RowChange::Tombstone {
                base_hash: item_hash(b)?,
            }],
            "hidden",
        ),
        Some(b) => {
            let mine: Option<Map<String, Value>> = current
                .as_ref()
                .filter(|(kind, _)| kind != TOMBSTONE)
                .and_then(|(_, item)| item.as_ref())
                .and_then(Value::as_object)
                .cloned();
            let mut item: Map<String, Value> = mine.unwrap_or_else(|| b.clone());
            if let Some(l) = p.label {
                item.insert("label".to_owned(), Value::String(l));
            }
            match p.description {
                Some(d) if d.is_empty() => {
                    item.remove("description");
                }
                Some(d) => {
                    item.insert("description".to_owned(), Value::String(d));
                }
                None => {}
            }
            item.insert("key".to_owned(), Value::String(key.to_owned()));
            let what = if hidden_now { "shown again" } else { "changed" };
            if &item == b {
                (vec![RowChange::Remove], what)
            } else {
                let content_hash = item_hash(&item)?;
                let item = Value::Object(item);
                let changes = match current.as_ref().map(|(kind, _)| kind.as_str()) {
                    Some(TOMBSTONE) => vec![
                        RowChange::Remove,
                        RowChange::Override {
                            item,
                            base_hash: item_hash(b)?,
                            content_hash,
                        },
                    ],
                    Some(_) => vec![RowChange::Content { item, content_hash }],
                    None => vec![RowChange::Override {
                        item,
                        base_hash: item_hash(b)?,
                        content_hash,
                    }],
                };
                (changes, what)
            }
        }
    };
    let at = RowAt {
        job: j.job_id,
        object: GATE_OPTION.to_owned(),
        key: key.to_owned(),
        by: j.t.caller.sub.clone(),
    };
    let w = DraftWrite {
        area: "gates",
        page: "J7",
        kind: "overlay",
        r#ref: place_ref(&[("object", GATE_OPTION), ("key", key)]),
        label: Label::Now(format!("Gate option {key} {what}")),
    };
    let out = design::track::<(), _>(&mut *c, reg, j, w, move |c| {
        Box::pin(async move {
            for change in changes {
                write_overlay(&mut *c, at.clone(), change, None).await?;
            }
            Ok::<(), ApiError>(())
        })
    })
    .await?;
    Ok(out.draft_rev)
}

// ---- B5 addOntologyAttribute (base entities) ------------------------------------------------------------------------

/// The object B5's `addOntologyAttribute` writes for an attribute of a BASE entity (key `<Entity>.<attr>`, B5 plan R2).
pub const ONTOLOGY_ATTRIBUTE: &str = "ontology_attribute";

/// One attribute B5 adds to a base entity ([`save_attribute_extension`]).
pub struct AttributeExtension<'a> {
    pub entity: &'a str,
    pub name: &'a str,
    pub data_type: &'a str,
    pub pii: bool,
    pub synonyms: &'a [String],
}

/// B5 `addOntologyAttribute` on a BASE entity (`POST /api/jobs/{job}/ontology/entities/{name}/attributes`, B5 plan R2
/// and T6), in the caller's job-scoped transaction: an `extension` row of object `ontology_attribute`, key
/// `<entity>.<name>`, its item in the bundle's attribute shape (`{key, label, type, pii, synonyms}`,
/// `release::compile` `attribute_item`). The caller has validated the name and checked that the entity is a base
/// entity of the draft and has no attribute `name`. One tracked write, kind `overlay`, area `ontology`, page J11.
/// Returns the job's `draft_rev`. Refusal (nothing is written): 409 `attribute_exists` when the place already holds a
/// row of the tenant's (an extension, an override or a tombstone of a base attribute).
pub async fn save_attribute_extension(
    c: &mut PgConnection,
    reg: &Registry,
    j: &JobCtx,
    a: AttributeExtension<'_>,
) -> ApiResult<i64> {
    let AttributeExtension {
        entity,
        name,
        data_type,
        pii,
        synonyms,
    } = a;
    let key = format!("{entity}.{name}");
    lock_job(&mut *c, j.job_id).await?;
    let taken: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM job_overlay_row WHERE job_id = $1 AND object = $2 AND key = $3)",
    )
    .bind(j.job_id)
    .bind(ONTOLOGY_ATTRIBUTE)
    .bind(&key)
    .fetch_one(&mut *c)
    .await?;
    if taken {
        return Err(ApiError::Conflict(
            "attribute_exists",
            format!("{entity} already has {name}."),
        ));
    }
    let mut item = OverlayItem::new();
    item.insert("key".to_owned(), Value::String(key.clone()));
    item.insert("label".to_owned(), Value::String(key.clone()));
    item.insert("type".to_owned(), Value::String(data_type.to_owned()));
    item.insert("pii".to_owned(), Value::Bool(pii));
    item.insert("synonyms".to_owned(), json!(synonyms));
    let content_hash = item_hash(&item)?;
    let item = Value::Object(item);
    let job = j.job_id;
    let by = j.t.caller.sub.clone();
    let row_key = key.clone();
    let w = DraftWrite {
        area: "ontology",
        page: "J11",
        kind: "overlay",
        r#ref: place_ref(&[("object", ONTOLOGY_ATTRIBUTE), ("key", key.as_str())]),
        label: Label::Now(format!("{entity}: attribute {name} added")),
    };
    let out = design::track::<(), _>(&mut *c, reg, j, w, move |c| {
        Box::pin(async move {
            sqlx::query(
                "INSERT INTO job_overlay_row \
                        (job_id, object, key, kind, item, base_hash, content_hash, updated_by) \
                 VALUES ($1, $2, $3, 'extension', $4, NULL, $5, $6)",
            )
            .bind(job)
            .bind(ONTOLOGY_ATTRIBUTE)
            .bind(&row_key)
            .bind(&item)
            .bind(&content_hash)
            .bind(&by)
            .execute(&mut *c)
            .await?;
            Ok::<(), ApiError>(())
        })
    })
    .await?;
    Ok(out.draft_rev)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_names_follow_the_fixture_pattern() {
        let longest = format!("a{}", "b".repeat(63));
        let too_long = format!("a{}", "b".repeat(64));
        for ok in ["a", "refund_reason", "x9", longest.as_str()] {
            assert!(is_slot(ok), "{ok}");
        }
        for bad in [
            "",
            "Order",
            "9a",
            "_a",
            "refund-reason",
            "refund reason",
            too_long.as_str(),
        ] {
            assert!(!is_slot(bad), "{bad}");
        }
    }

    #[test]
    fn only_tone_escalation_and_format_are_refinable() {
        for slot in ["tone", "escalation", "format"] {
            assert!(check_refinable(INSTRUCTION, slot).is_ok(), "{slot}");
        }
        for slot in ["identity", "policy", "safety", "persona"] {
            let e = check_refinable(INSTRUCTION, slot).unwrap_err();
            assert_eq!(e.code(), "instruction_not_refinable", "{slot}");
        }
        assert!(check_refinable(CONTEXT_SPACE, "policy").is_ok());
        assert!(OVERLAY_OBJECTS.contains(&INSTRUCTION));
    }

    #[test]
    fn ops_parse_and_label() {
        assert_eq!(Op::parse("hide").unwrap(), Op::Hide);
        assert_eq!(Op::parse("rebase").unwrap().done(), "kept on the new base");
        let e = Op::parse("delete").unwrap_err();
        assert_eq!(e.code(), "invalid_op");
    }
}
