//! Knowledge kinds of a job (B3 T10; plan §4 0054, §5.5, §0 R9/R10/R11; D54, D68): the working rows of design kind
//! `knowledge` (job scope, key `kind:<stage>:<kind>`, value `{store, packs}`), their template seeds, the platform-pack
//! cache and the kind-scoped search over a leaf set.
//!
//! - **Value** (R10): `{store: "<path>" | null, packs: ["<key>@<version>", …]}`, packs sorted by key, one version per
//!   key. A kind is bound when it has a store or at least one pack. No row = no place (`None`).
//! - **Writers** (tests/design_antidrift.rs: this module is the kind's targets module): [`KnowledgeTarget::set`]
//!   (discard, follow-live, restore), [`bind_row`] (only inside the `design::track` closure of `bindKnowledgeKind`)
//!   and [`materialise_from_template`] (job creation, the B2 `PinsAdopted` hook and a job's stack move: template rows,
//!   `origin = 'base'`; a row's `store_path` and `origin` are kept, `packs` only follow the template while
//!   `packs_source = 'template'`; kinds the new template no longer asks for are demoted, never deleted —
//!   [`demote_absent_kinds`]).
//! - **Seeds** are the job's effective knowledge kinds (D107, KH T3): the `stage_knowledge_kind` rows of every pinned
//!   layer the selection applies — base, industry, each department with its archetype's kinds, job — folded in THE
//!   fold order (`agent_model::compile::fold_knowledge`, the compile's own), the union over the departments (D115)
//!   ([`template_seeds`]); `required` only ever false → true down the fold, `tenant_data` (D110) carried along. A
//!   seed's pack pins are the pinned tags' `knowledge_pack` rows folded for its kind, minus the job's
//!   `knowledge_pack` refinement tombstones (PK-1/PK-2, [`packs::template`](super::packs::template)).
//! - **Platform packs** (R10): every `GET /api/platform/packs` answer is written to `platform_pack`
//!   ([`record_listing`]); a pin's leaves come from the release snapshot or the leaf cache (PK-7). A query that finds
//!   a pinned pack unreadable marks it ([`mark_unreadable`]) and degrades by pin (R11, [`search`]).
//! - **Leaf set** ([`leaf_set`]): the kind's tenant leaf (by path) plus its pinned packs' leaves, per [`DesignOf`]
//!   (`release_pack_leaf` for a release, `platform_pack_leaf` for the draft). The tenant leaf's strata store is the
//!   one of the job's PINNED generation of its root (T4a, [`generations::strata_for`]), as readiness, coverage and
//!   analytics resolve it. Searches send `leaf_ids` only, with the tenant's `supersedes` (PK-8).
//! - **Backfill** ([`backfill`]): at boot, jobs that predate B3 (no kind rows) get their template's kinds.
use std::collections::BTreeMap;
use std::sync::Arc;

use agent_model::compile::Selection;
use agent_model::kinds::knowledge_pack;
use chrono::{DateTime, Utc};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use super::generations;
use super::packs::resolve;
use super::packs::template as template_packs;
use super::stores::{self, LeafSetAnswer, PackLeaves, StoreRow};
use crate::design::{self, DesignOf, DesignTarget, DraftRef, JobRef, Place, Registry, Scope, Snap};
use crate::error::{ApiError, ApiResult};
use crate::platform::templates::TemplatePin;
use crate::release::compile::{Compiled, PinsAdopted};
use crate::release::targets::SET_BY;
use crate::state::AppState;
use crate::strata::rag::{self, PlatformPack, QueryReq};
use crate::strata::{self, Service, StrataStoreId};
use crate::tenancy::begin_scoped;

/// The ledger kind.
pub const KIND: &str = "knowledge";
/// The most pins one kind holds (the 0054 CHECK).
pub const MAX_PINS: usize = 16;

/// Register the `knowledge` target (one line in `design::registry()`).
pub fn register(r: &mut Registry) {
    r.register(Arc::new(KnowledgeTarget));
}

fn internal(what: String) -> ApiError {
    ApiError::Internal(anyhow::anyhow!(what))
}

/// 404 `kind_not_found` (fixture `bindKnowledgeKind`).
pub fn kind_not_found(stage: &str, kind: &str) -> ApiError {
    ApiError::Missing(
        "kind_not_found",
        format!("No knowledge kind {kind} in {stage}."),
    )
}

fn invalid_pack(msg: String) -> ApiError {
    ApiError::Unprocessable("invalid_pack", msg)
}

// ---------------------------------------------------------------------------------------------------------------
// Pins

/// One platform pack pin, `key@version`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct PackPin {
    pub key: String,
    pub version: i32,
}

impl PackPin {
    /// `"<key>@<version>"`.
    pub fn pin(&self) -> String {
        format!("{}@{}", self.key, self.version)
    }
}

/// A pack key: one to six `/`-separated segments of `[a-z0-9_-]` (the 0054 CHECK).
pub fn valid_pack_key(key: &str) -> bool {
    let segments: Vec<&str> = key.split('/').collect();
    segments.len() <= 6
        && segments.iter().all(|s| {
            !s.is_empty()
                && s.bytes().all(|b| {
                    b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-')
                })
        })
}

/// `key@version` (version 1..=999999999, no leading zero), else `None`.
pub fn parse_pin(s: &str) -> Option<PackPin> {
    let (key, version) = s.rsplit_once('@')?;
    let digits = version.as_bytes();
    let well_formed = (1..=9).contains(&digits.len())
        && digits[0] != b'0'
        && digits.iter().all(u8::is_ascii_digit);
    if !well_formed || !valid_pack_key(key) {
        return None;
    }
    Some(PackPin {
        key: key.to_string(),
        version: version.parse().ok()?,
    })
}

/// The pins of a bind: at most [`MAX_PINS`], each `key@version`, no key twice; sorted by key. 422 `invalid_pack`.
pub fn check_pins(pins: &[String]) -> ApiResult<Vec<PackPin>> {
    if pins.len() > MAX_PINS {
        return Err(invalid_pack(format!(
            "A knowledge kind pins at most {MAX_PINS} packs."
        )));
    }
    let mut out: Vec<PackPin> = Vec::with_capacity(pins.len());
    for p in pins {
        let Some(pin) = parse_pin(p.trim()) else {
            return Err(invalid_pack(format!(
                "“{p}” is not a pack pin (key@version)."
            )));
        };
        if out.iter().any(|o| o.key == pin.key) {
            return Err(invalid_pack(format!(
                "Pack {} is pinned more than once.",
                pin.key
            )));
        }
        out.push(pin);
    }
    out.sort();
    Ok(out)
}

// ---------------------------------------------------------------------------------------------------------------
// Values and working rows

/// The ref `{stage, kind}` of a kind.
pub fn kind_ref(stage: &str, kind: &str) -> DraftRef {
    let mut m = DraftRef::new();
    m.insert("stage".to_string(), Value::String(stage.to_string()));
    m.insert("kind".to_string(), Value::String(kind.to_string()));
    m
}

fn ref_part<'r>(r: &'r DraftRef, f: &str) -> &'r str {
    r.get(f).and_then(Value::as_str).unwrap_or("")
}

/// The design value `{store, packs}`.
pub fn kind_value(store: Option<&str>, packs: &[String]) -> Value {
    json!({"store": store, "packs": packs})
}

/// `(store, packs)` of a design value; anything else is a corrupt value (500).
pub fn value_parts(v: &Value) -> ApiResult<(Option<String>, Vec<String>)> {
    let store = match v.get("store") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(other) => {
            return Err(internal(format!(
                "a knowledge value's store is not a string: {other}"
            )));
        }
    };
    let packs = match v.get("packs") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(a)) => a
            .iter()
            .map(|p| {
                p.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| internal(format!("a knowledge pack pin is not a string: {p}")))
            })
            .collect::<ApiResult<Vec<String>>>()?,
        Some(other) => {
            return Err(internal(format!(
                "a knowledge value's packs is not a list: {other}"
            )));
        }
    };
    Ok((store, packs))
}

/// The one-line text of a value: `faq/policy + base@1`, or `unbound` (fixture `text`).
pub fn value_text(v: &Snap) -> String {
    let Some(v) = v else {
        return "unbound".to_string();
    };
    let (store, packs) = value_parts(v).unwrap_or_default();
    let parts: Vec<String> = store.into_iter().chain(packs).collect();
    if parts.is_empty() {
        "unbound".to_string()
    } else {
        parts.join(" + ")
    }
}

/// One `job_knowledge_kind` row.
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct KindRow {
    pub stage: String,
    pub kind: String,
    pub required: bool,
    /// D110 (0135): the kind is the tenant's own data; frozen packs alone do not satisfy it.
    pub tenant_data: bool,
    pub description: String,
    pub origin: String,
    pub store_path: Option<String>,
    pub packs: Vec<String>,
    pub packs_source: String,
    pub updated_at: DateTime<Utc>,
}

impl KindRow {
    /// The row's design value.
    pub fn value(&self) -> Value {
        kind_value(self.store_path.as_deref(), &self.packs)
    }
}

const KIND_COLUMNS: &str = "SELECT stage, kind, required, tenant_data, description, origin, store_path, packs, \
     packs_source, updated_at FROM job_knowledge_kind";

/// Every kind of the job (scoped tenant), by stage then kind.
pub async fn rows(c: &mut PgConnection, job: Uuid) -> ApiResult<Vec<KindRow>> {
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{KIND_COLUMNS} WHERE job_id = $1 ORDER BY stage, kind"
    )))
    .bind(job)
    .fetch_all(&mut *c)
    .await?)
}

/// One kind of the job.
pub async fn row(
    c: &mut PgConnection,
    job: Uuid,
    stage: &str,
    kind: &str,
) -> ApiResult<Option<KindRow>> {
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{KIND_COLUMNS} WHERE job_id = $1 AND stage = $2 AND kind = $3"
    )))
    .bind(job)
    .bind(stage)
    .bind(kind)
    .fetch_optional(&mut *c)
    .await?)
}

/// `bindKnowledgeKind`'s write: `store` absent = keep, `Some(None)` = unbind the tenant leaf; `packs` absent = keep,
/// given = these pins (`packs_source = 'binding'`). Only inside the `design::track` closure of the bind.
pub async fn bind_row(
    c: &mut PgConnection,
    job: Uuid,
    stage: &str,
    kind: &str,
    store: Option<Option<String>>,
    packs: Option<Vec<String>>,
    by: &str,
) -> ApiResult<()> {
    let set_store = store.is_some();
    let n = sqlx::query(
        "UPDATE job_knowledge_kind \
            SET store_path = CASE WHEN $4 THEN $5 ELSE store_path END, \
                packs = coalesce($6::text[], packs), \
                packs_source = CASE WHEN $6::text[] IS NULL THEN packs_source ELSE 'binding' END, \
                updated_by = $7, updated_at = clock_timestamp() \
          WHERE job_id = $1 AND stage = $2 AND kind = $3",
    )
    .bind(job)
    .bind(stage)
    .bind(kind)
    .bind(set_store)
    .bind(store.flatten())
    .bind(packs)
    .bind(by)
    .execute(&mut *c)
    .await?
    .rows_affected();
    if n == 0 {
        return Err(kind_not_found(stage, kind));
    }
    Ok(())
}

/// Restore path of [`KnowledgeTarget::set`]: the row holds exactly `(store, packs)` (created as a tenant row when
/// missing). `packs_source` turns `binding` when the pins change.
async fn upsert_value(
    c: &mut PgConnection,
    job: Uuid,
    stage: &str,
    kind: &str,
    store: Option<&str>,
    packs: &[String],
) -> ApiResult<()> {
    sqlx::query(
        "INSERT INTO job_knowledge_kind (job_id, stage, kind, store_path, packs, updated_by) \
         VALUES ($1, $2, $3, $4, $5, $6) \
         ON CONFLICT (tenant_id, job_id, stage, kind) DO UPDATE \
            SET store_path = EXCLUDED.store_path, packs = EXCLUDED.packs, \
                packs_source = CASE WHEN job_knowledge_kind.packs = EXCLUDED.packs \
                                    THEN job_knowledge_kind.packs_source ELSE 'binding' END, \
                updated_by = EXCLUDED.updated_by, updated_at = clock_timestamp()",
    )
    .bind(job)
    .bind(stage)
    .bind(kind)
    .bind(store)
    .bind(packs)
    .bind(SET_BY)
    .execute(&mut *c)
    .await?;
    Ok(())
}

async fn delete_row(c: &mut PgConnection, job: Uuid, stage: &str, kind: &str) -> ApiResult<()> {
    sqlx::query("DELETE FROM job_knowledge_kind WHERE job_id = $1 AND stage = $2 AND kind = $3")
        .bind(job)
        .bind(stage)
        .bind(kind)
        .execute(&mut *c)
        .await?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------------------------
// The design target

/// `knowledge` (D68, R10): which tenant leaf and which platform packs one knowledge kind of a stage reads.
pub struct KnowledgeTarget;

fn job_of(at: Place<'_>) -> ApiResult<Uuid> {
    at.job
        .map(|j| j.id)
        .ok_or_else(|| internal("the knowledge target is read and written at a job place".into()))
}

impl DesignTarget for KnowledgeTarget {
    fn kind(&self) -> &'static str {
        KIND
    }
    fn scope(&self) -> Scope {
        Scope::Job
    }
    fn key(&self, r: &DraftRef) -> String {
        format!("kind:{}:{}", ref_part(r, "stage"), ref_part(r, "kind"))
    }
    fn get<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
        r: &'a DraftRef,
    ) -> BoxFuture<'a, ApiResult<Snap>> {
        Box::pin(async move {
            let job = job_of(at)?;
            let found = row(c, job, ref_part(r, "stage"), ref_part(r, "kind")).await?;
            Ok(found.map(|k| k.value()))
        })
    }
    fn set<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
        r: &'a DraftRef,
        v: &'a Snap,
    ) -> BoxFuture<'a, ApiResult<()>> {
        Box::pin(async move {
            let job = job_of(at)?;
            let (stage, kind) = (ref_part(r, "stage"), ref_part(r, "kind"));
            match v {
                None => delete_row(c, job, stage, kind).await,
                Some(v) => {
                    let (store, packs) = value_parts(v)?;
                    upsert_value(c, job, stage, kind, store.as_deref(), &packs).await
                }
            }
        })
    }
    fn places<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
    ) -> BoxFuture<'a, ApiResult<Option<Vec<DraftRef>>>> {
        Box::pin(async move {
            let job = job_of(at)?;
            let refs: Vec<(String, String)> = sqlx::query_as(
                "SELECT stage, kind FROM job_knowledge_kind WHERE job_id = $1 ORDER BY stage, kind",
            )
            .bind(job)
            .fetch_all(&mut *c)
            .await?;
            Ok(Some(
                refs.iter()
                    .map(|(stage, kind)| kind_ref(stage, kind))
                    .collect(),
            ))
        })
    }
    fn text(&self, v: &Snap, _r: &DraftRef) -> Option<String> {
        Some(value_text(v))
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Template seeds and materialise

/// One knowledge kind a template asks a stage for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KindSeed {
    /// A stage, or `*` (every stage, D107).
    pub stage: String,
    pub kind: String,
    pub required: bool,
    /// D110: the tenant's own data (frozen packs alone do not satisfy it).
    pub tenant_data: bool,
    pub description: String,
    /// The template's pack pins for the kind (`key@version`, sorted by key, at most [`MAX_PINS`]; PK-1/PK-2).
    pub packs: Vec<String>,
}

/// The pack pins a template sets on a seed's kind (`packs_source = 'template'`): [`KindSeed::packs`].
pub fn template_pack_pins(seed: &KindSeed) -> Vec<String> {
    seed.packs.clone()
}

/// One `stage` / `kind` part as the 0054 CHECKs take it: `1..=max` characters, no `:`, `|` or whitespace.
fn seed_part_ok(s: &str, max: usize) -> bool {
    (1..=max).contains(&s.chars().count())
        && !s.chars().any(|c| c == ':' || c == '|' || c.is_whitespace())
}

/// `(stage, kind)` of a `stage_knowledge_kind` row key `stage|kind` when the 0054 CHECKs accept both parts (stage
/// 1-64 characters, kind 1-128, neither with `:`, `|` or whitespace); `None` otherwise. The layer validator
/// (`agent_model::kinds::split_stage_knowledge_key`) is stricter, so a row saved through it always passes.
pub fn seed_parts(key: &str) -> Option<(&str, &str)> {
    let (stage, kind) = key.split_once('|')?;
    (seed_part_ok(stage, 64) && seed_part_ok(kind, 128)).then_some((stage, kind))
}

/// The kind seeds of a template: the job's effective knowledge kinds over the pinned tags under `sel`
/// ([`template_packs::knowledge`]: D107/D112/D115 — base, industry, each selected department after its archetype,
/// the job; the union over the departments), by key. `required` / `description` / `tenant_data` are the folded
/// kind's. A kind whose key the 0054 CHECKs would refuse ([`seed_parts`]) is skipped with a warning: bad template
/// data never fails job creation or an approve. No industry, no department or no pin: no seeds.
///
/// `packs` = the `knowledge_pack` fold of the pinned tags for the seed's kind ([`packs::template::pins_for`]),
/// minus `job`'s `knowledge_pack` refinement tombstones (none without a job: the compiler's overlay base).
///
/// [`packs::template::pins_for`]: super::packs::template::pins_for
pub async fn template_seeds(
    c: &mut PgConnection,
    pins: &[TemplatePin],
    sel: Option<&Selection>,
    job: Option<&JobRef>,
) -> ApiResult<Vec<KindSeed>> {
    let Some(sel) = sel else {
        return Ok(Vec::new());
    };
    if sel.industry.is_empty() || pins.is_empty() || sel.departments.is_empty() {
        return Ok(Vec::new());
    }
    let knowledge = template_packs::knowledge(&mut *c, pins, sel).await?;
    let mut out = Vec::with_capacity(knowledge.kinds.len());
    for k in &knowledge.kinds {
        let key = k.key();
        if seed_parts(&key).is_none() {
            // a row the 0054 CHECKs would refuse never fails job creation or an approve: skipped, loudly
            tracing::warn!(key = %key, "stage_knowledge_kind key is not a valid stage|kind; skipped");
            continue;
        }
        out.push(KindSeed {
            stage: k.stage.clone(),
            kind: k.kind.clone(),
            required: k.required,
            tenant_data: k.tenant_data,
            description: k.description.clone(),
            packs: Vec::new(),
        });
    }
    if out.is_empty() {
        return Ok(out);
    }
    let refs = template_packs::refs_of(&knowledge);
    let tombstones = match job {
        Some(j) => template_packs::job_tombstones(&mut *c, j.id).await?,
        None => Vec::new(),
    };
    for s in &mut out {
        s.packs = template_packs::pins_for(&refs, &s.stage, &s.kind, &tombstones);
    }
    Ok(out)
}

/// D107: the pack references of the job's draft template (its pins and draft selection) that match none of its
/// effective knowledge kinds — `pack_not_applicable`, listed in the job's knowledge view, never an error.
pub async fn not_applicable_packs(
    c: &mut PgConnection,
    job: &JobRef,
) -> ApiResult<Vec<agent_model::compile::FoldedPack>> {
    Ok(template_knowledge(c, job)
        .await?
        .map(|k| k.not_applicable)
        .unwrap_or_default())
}

/// The knowledge fold of the job's draft template (its pins and draft selection): every pack reference it inherits,
/// each with the layer that set it (base → industry → department, D112/D115), and the `not_applicable` ones. `None`
/// while the job has no draft selection.
pub async fn template_knowledge(
    c: &mut PgConnection,
    job: &JobRef,
) -> ApiResult<Option<agent_model::compile::CompiledKnowledge>> {
    let (pins, sel) = draft_template(&mut *c, job).await?;
    let Some(sel) = sel else {
        return Ok(None);
    };
    Ok(Some(template_packs::knowledge(&mut *c, &pins, &sel).await?))
}

/// Write the template's kinds into the job's working rows (idempotent): a new kind is inserted (`origin = 'base'`,
/// `packs` = [`template_pack_pins`] (the seed's), `packs_source = 'template'`); an existing one keeps its `store_path` and
/// `origin`, takes the template's `required` / `tenant_data` / `description`, and takes its pins only while
/// `packs_source = 'template'` (a `binding` row's pins are never overwritten). This is a PARTIAL upsert: kinds the seed
/// set does not name are left alone, so a caller that is writing a whole template follows it with
/// [`demote_absent_kinds`] ([`materialise_from_template`] does).
pub async fn materialise(tx: &mut PgConnection, job: &JobRef, seeds: &[KindSeed]) -> ApiResult<()> {
    for s in seeds {
        let packs = template_pack_pins(s);
        sqlx::query(
            "INSERT INTO job_knowledge_kind \
                    (job_id, stage, kind, required, tenant_data, description, origin, packs, packs_source, \
                     updated_by) \
             VALUES ($1, $2, $3, $4, $5, $6, 'base', $7, 'template', $8) \
             ON CONFLICT (tenant_id, job_id, stage, kind) DO UPDATE \
                SET required = EXCLUDED.required, tenant_data = EXCLUDED.tenant_data, \
                    description = EXCLUDED.description, \
                    packs = CASE WHEN job_knowledge_kind.packs_source = 'template' \
                                 THEN EXCLUDED.packs ELSE job_knowledge_kind.packs END",
        )
        .bind(job.id)
        .bind(&s.stage)
        .bind(&s.kind)
        .bind(s.required)
        .bind(s.tenant_data)
        .bind(&s.description)
        .bind(packs)
        .bind(SET_BY)
        .execute(&mut *tx)
        .await?;
    }
    Ok(())
}

/// Demote, never delete, the job's kinds the new template no longer names (`seeds`): `required = false`, and
/// `packs = '{}'` only while `packs_source = 'template'` (the old template's pins). A `binding` row keeps its packs —
/// they are the tenant's own choice — and every row keeps its `store_path`, `origin` and `tenant_data`, so a later
/// template that asks for the kind again finds the tenant's binding intact. A row that is already demoted and free of
/// template pins is not touched (idempotent). A seed's stage `*` covers every stage of its kind (D107: an archetype
/// kind is folded as `*|<kind>`), so a concrete row a wildcard seed still names is kept. An empty seed list means the
/// job has no template to compare against (no selection, no pins, or unreadable template data), so nothing is
/// demoted — otherwise `unnest` of two empty arrays would demote every row. The second step of
/// [`materialise_from_template`].
pub async fn demote_absent_kinds(
    tx: &mut PgConnection,
    job: &JobRef,
    seeds: &[KindSeed],
) -> ApiResult<()> {
    if seeds.is_empty() {
        return Ok(());
    }
    let stages: Vec<String> = seeds.iter().map(|s| s.stage.clone()).collect();
    let kinds: Vec<String> = seeds.iter().map(|s| s.kind.clone()).collect();
    sqlx::query(
        "UPDATE job_knowledge_kind k \
            SET required = false, \
                packs = CASE WHEN k.packs_source = 'template' THEN '{}'::text[] ELSE k.packs END, \
                updated_by = $2, updated_at = clock_timestamp() \
          WHERE k.job_id = $1 \
            AND (k.required OR (k.packs_source = 'template' AND cardinality(k.packs) > 0)) \
            AND NOT EXISTS (SELECT 1 FROM unnest($3::text[], $4::text[]) AS s(stage, kind) \
                            WHERE s.kind = k.kind AND (s.stage = '*' OR s.stage = k.stage))",
    )
    .bind(job.id)
    .bind(SET_BY)
    .bind(stages.as_slice())
    .bind(kinds.as_slice())
    .execute(&mut *tx)
    .await?;
    Ok(())
}

/// [`template_seeds`] then [`materialise`] then [`demote_absent_kinds`]: the job-creation hook's call and every stack
/// change (a moved target, an adopted pin set). The seeds are the new template's kinds; the kinds it no longer asks for
/// are demoted, never deleted, so a move to another department cannot leave a stale `required` kind blocking
/// readiness.
pub async fn materialise_from_template(
    tx: &mut PgConnection,
    job: &JobRef,
    pins: &[TemplatePin],
    sel: Option<&Selection>,
) -> ApiResult<()> {
    let seeds = template_seeds(&mut *tx, pins, sel, Some(job)).await?;
    materialise(&mut *tx, job, &seeds).await?;
    demote_absent_kinds(tx, job, &seeds).await
}

/// After a `knowledge_pack` refinement tombstone of `job` was written or removed (`overlay_key` =
/// `stage|kind|pack_key`, either part may be `*`): the template-sourced pins of the kinds that key covers follow the
/// template again (the job's draft pins and selection; its tombstones as they are now). Runs inside the overlay
/// write's `design::track` closure (tests/design_antidrift.rs: this module is the kind's targets module). `binding`
/// rows and kinds the template does not seed are left alone; a key that is not a `knowledge_pack` key changes nothing.
pub async fn refresh_template_packs(
    c: &mut PgConnection,
    job: &JobRef,
    overlay_key: &str,
) -> ApiResult<()> {
    let Some((stage, kind, _)) = knowledge_pack::split_key(overlay_key) else {
        return Ok(());
    };
    let any = knowledge_pack::ANY;
    let (pins, sel) = draft_template(&mut *c, job).await?;
    let seeds = template_seeds(&mut *c, &pins, sel.as_ref(), Some(job)).await?;
    for s in seeds
        .iter()
        .filter(|s| (stage == any || s.stage == stage) && (kind == any || s.kind == kind))
    {
        sqlx::query(
            "UPDATE job_knowledge_kind SET packs = $4, updated_by = $5, updated_at = clock_timestamp() \
              WHERE job_id = $1 AND stage = $2 AND kind = $3 AND packs_source = 'template' \
                AND packs IS DISTINCT FROM $4",
        )
        .bind(job.id)
        .bind(&s.stage)
        .bind(&s.kind)
        .bind(&s.packs)
        .bind(SET_BY)
        .execute(&mut *c)
        .await?;
    }
    Ok(())
}

/// The job's pins (`jobs.pins`) and draft selection (`job_target.selection`).
async fn draft_template(
    c: &mut PgConnection,
    job: &JobRef,
) -> ApiResult<(Vec<TemplatePin>, Option<Selection>)> {
    let pins: Value = sqlx::query_scalar("SELECT pins FROM jobs WHERE id = $1")
        .bind(job.id)
        .fetch_one(&mut *c)
        .await?;
    let pins: Vec<TemplatePin> = serde_json::from_value(pins)
        .map_err(|e| internal(format!("jobs.pins of {} is unreadable: {e}", job.key)))?;
    let sel = stored_selection(&mut *c, job).await?;
    Ok((pins, sel))
}

/// `job_target.selection` of the job (`None` without a row or a selection).
async fn stored_selection(c: &mut PgConnection, job: &JobRef) -> ApiResult<Option<Selection>> {
    let stored: Option<Option<Value>> =
        sqlx::query_scalar("SELECT selection FROM job_target WHERE job_id = $1")
            .bind(job.id)
            .fetch_optional(&mut *c)
            .await?;
    stored
        .flatten()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|e| internal(format!("job_target.selection is unreadable: {e}")))
}

/// The B2 `PinsAdopted` hook (§0 R9): adopted pins re-materialise the job's kinds from the new template tags. The
/// seeds are recomputed over the NEW pins, so template-sourced pack pins follow the adopted tags' `knowledge_pack`
/// rows, minus the job's `knowledge_pack` tombstones (which survive an adopt: their key has no version, PK-2).
pub struct Rematerialise;

/// [`Rematerialise`] as `AppState.pins_adopted` holds it.
pub fn pins_adopted_hook() -> Arc<dyn PinsAdopted> {
    Arc::new(Rematerialise)
}

impl PinsAdopted for Rematerialise {
    fn on_pins_adopted<'a>(
        &'a self,
        c: &'a mut PgConnection,
        job: &'a JobRef,
        _old: &'a [TemplatePin],
        new: &'a [TemplatePin],
        _compiled: &'a Compiled,
    ) -> BoxFuture<'a, ApiResult<()>> {
        Box::pin(async move {
            let sel = stored_selection(&mut *c, job).await?;
            materialise_from_template(c, job, new, sel.as_ref()).await
        })
    }
}

// ---------------------------------------------------------------------------------------------------------------
// The platform-pack cache (R10)

/// One `platform_pack` row.
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct CachedPack {
    pub pack_key: String,
    pub version: i32,
    pub root_id: Uuid,
    pub published_at: Option<DateTime<Utc>>,
    pub listed: bool,
    pub checked_at: DateTime<Utc>,
    pub unreadable_at: Option<DateTime<Utc>>,
    /// The platform owner's classification (PK-5): `published | retired | draft | missing`; `None` = not classified.
    pub state: Option<String>,
    /// The tenant's exclusion prefix that hides the pack (PK-10); `None` = not excluded.
    pub excluded_by: Option<String>,
}

impl CachedPack {
    pub fn pin(&self) -> String {
        format!("{}@{}", self.pack_key, self.version)
    }

    /// Known unsearchable without asking strata again: marked unreadable, excluded, or classified missing / draft.
    pub fn unusable(&self) -> bool {
        self.unreadable_at.is_some()
            || self.excluded_by.is_some()
            || matches!(self.state.as_deref(), Some("missing" | "draft"))
    }
}

/// Every cached pack of the scoped tenant, by key then version.
pub async fn cached_packs(c: &mut PgConnection) -> ApiResult<Vec<CachedPack>> {
    Ok(sqlx::query_as(
        "SELECT pack_key, version, root_id, published_at, listed, checked_at, unreadable_at, state, excluded_by \
           FROM platform_pack ORDER BY pack_key, version",
    )
    .fetch_all(&mut *c)
    .await?)
}

/// The number of cached leaves (`platform_pack_leaf`) per pin `"<key>@<version>"` of the scoped tenant.
pub async fn cached_leaf_counts(c: &mut PgConnection) -> ApiResult<BTreeMap<String, i64>> {
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT pack_key || '@' || version::text, count(*) FROM platform_pack_leaf GROUP BY pack_key, version",
    )
    .fetch_all(&mut *c)
    .await?;
    Ok(rows.into_iter().collect())
}

/// The packs of a listing the tenant may pin: not retired, with a well-formed key.
pub fn pinnable(listing: &[PlatformPack]) -> impl Iterator<Item = &PlatformPack> {
    listing
        .iter()
        .filter(|p| p.retired_at.is_none() && p.version >= 1 && valid_pack_key(&p.key))
}

/// Write one `GET /api/platform/packs` answer into the cache: each listed pack upserted `listed = true` (an earlier
/// unreadable mark is cleared: the tenant may read it again), every other cached pack of the tenant `listed = false`.
pub async fn record_listing(
    c: &mut PgConnection,
    tenant: Uuid,
    listing: &[PlatformPack],
) -> ApiResult<()> {
    let mut pins: Vec<String> = Vec::new();
    for p in pinnable(listing) {
        let published: Option<DateTime<Utc>> = p
            .published_at
            .as_deref()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.with_timezone(&Utc));
        sqlx::query(
            "INSERT INTO platform_pack (tenant_id, pack_key, version, root_id, published_at, listed, checked_at) \
             VALUES ($1, $2, $3, $4, $5, true, clock_timestamp()) \
             ON CONFLICT (tenant_id, pack_key, version) DO UPDATE \
                SET root_id = EXCLUDED.root_id, published_at = EXCLUDED.published_at, listed = true, \
                    checked_at = EXCLUDED.checked_at, unreadable_at = NULL",
        )
        .bind(tenant)
        .bind(&p.key)
        .bind(p.version)
        .bind(p.root_id.0)
        .bind(published)
        .execute(&mut *c)
        .await?;
        pins.push(format!("{}@{}", p.key, p.version));
    }
    sqlx::query(
        "UPDATE platform_pack SET listed = false, checked_at = clock_timestamp() \
          WHERE NOT (pack_key || '@' || version::text = ANY($1::text[]))",
    )
    .bind(pins)
    .execute(&mut *c)
    .await?;
    Ok(())
}

/// R11: pins a query found unreadable (`listed = false`, `unreadable_at = now()`; an earlier mark is kept).
pub async fn mark_unreadable(c: &mut PgConnection, pins: &[String]) -> ApiResult<()> {
    if pins.is_empty() {
        return Ok(());
    }
    sqlx::query(
        "UPDATE platform_pack SET listed = false, unreadable_at = coalesce(unreadable_at, clock_timestamp()) \
          WHERE pack_key || '@' || version::text = ANY($1::text[])",
    )
    .bind(pins)
    .execute(&mut *c)
    .await?;
    Ok(())
}

/// The newest pinnable version of `pin`'s key when it is newer than the pin (`"<key>@<v>"`, R10 D3: shown, never
/// applied).
pub fn newer_of(pin: &str, listed: &[(String, i32)]) -> Option<String> {
    let p = parse_pin(pin)?;
    listed
        .iter()
        .filter(|(k, v)| *k == p.key && *v > p.version)
        .map(|(_, v)| *v)
        .max()
        .map(|v| format!("{}@{v}", p.key))
}

// ---------------------------------------------------------------------------------------------------------------
// Leaf sets and the kind-scoped search (R10/R11, PK-7, PK-8)

/// What a kind searches: its tenant leaf (the strata store of the job's pinned generation) and its pinned packs
/// resolved to their leaves (PK-7).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LeafSet {
    /// The tenant leaf's strata store in the job's pinned generation (`None` = unbound there).
    pub tenant: Option<StrataStoreId>,
    /// The tenant leaf's agent store id (`None` = no store bound, or its path is no leaf of the tree).
    pub tenant_store: Option<Uuid>,
    /// The pins with leaves to search, or known unsearchable (`unreadable`), in pin order.
    pub packs: Vec<PackLeaves>,
    /// Pins with no leaves in the design's source (snapshot or cache) and not known unsearchable: reported degraded
    /// (a draft search refreshes them from strata once first).
    pub unresolved: Vec<String>,
}

impl LeafSet {
    /// Nothing to send strata: no tenant leaf and no pack with a leaf to search.
    pub fn is_empty(&self) -> bool {
        self.tenant.is_none() && !self.packs.iter().any(PackLeaves::live)
    }

    /// Every pin of the set (`packs` then `unresolved`).
    pub fn pins(&self) -> Vec<String> {
        self.packs
            .iter()
            .map(|p| p.pin.clone())
            .chain(self.unresolved.iter().cloned())
            .collect()
    }
}

/// The leaf store at `path` (keys root-first joined by `/`), if the tree has one.
pub fn leaf_by_path<'s>(stores: &'s [StoreRow], path: &str) -> Option<&'s StoreRow> {
    stores
        .iter()
        .find(|s| s.is_leaf() && stores::path_of(stores, s.id).join("/") == path)
}

/// `(pin, leaf)` rows, grouped per pin by [`group_leaves`].
type PinLeaf = (String, Uuid);

fn group_leaves(rows: Vec<PinLeaf>) -> BTreeMap<String, Vec<StrataStoreId>> {
    let mut out: BTreeMap<String, Vec<StrataStoreId>> = BTreeMap::new();
    for (pin, leaf) in rows {
        out.entry(pin).or_default().push(StrataStoreId(leaf));
    }
    out
}

/// The leaves release `release` of `job` snapshotted for `pins` (`release_pack_leaf`, immutable), by leaf id.
async fn release_leaves(
    c: &mut PgConnection,
    job: Uuid,
    release: &str,
    pins: &[String],
) -> ApiResult<BTreeMap<String, Vec<StrataStoreId>>> {
    let rows: Vec<PinLeaf> = sqlx::query_as(
        "SELECT pin, leaf_id FROM release_pack_leaf \
          WHERE job_id = $1 AND release_id = $2 AND pin = ANY($3::text[]) ORDER BY pin, leaf_id",
    )
    .bind(job)
    .bind(release)
    .bind(pins)
    .fetch_all(&mut *c)
    .await?;
    Ok(group_leaves(rows))
}

/// The cached leaves of `pins` (`platform_pack_leaf`, the tenant's last `…/stores` answers), by leaf id.
async fn cached_leaves(
    c: &mut PgConnection,
    pins: &[String],
) -> ApiResult<BTreeMap<String, Vec<StrataStoreId>>> {
    let rows: Vec<PinLeaf> = sqlx::query_as(
        "SELECT pack_key || '@' || version::text AS pin, leaf_id FROM platform_pack_leaf \
          WHERE pack_key || '@' || version::text = ANY($1::text[]) ORDER BY pack_key, version, leaf_id",
    )
    .bind(pins)
    .fetch_all(&mut *c)
    .await?;
    Ok(group_leaves(rows))
}

/// The release a design reads pack leaves from: `Release(id)` itself, `Live` the job's current release (`None` =
/// never released), `Draft` the job's newest release (the fallback for pins the cache has no leaves of: published
/// packs never change per `key@version`, T4's cached-pin rule).
async fn leaf_release(
    c: &mut PgConnection,
    job: &JobRef,
    of: &DesignOf,
) -> ApiResult<Option<String>> {
    Ok(match of {
        DesignOf::Release(id) => Some(id.clone()),
        DesignOf::Live => sqlx::query_scalar::<_, Option<String>>(
            "SELECT current_release_id FROM jobs WHERE id = $1",
        )
        .bind(job.id)
        .fetch_optional(&mut *c)
        .await?
        .flatten(),
        DesignOf::Draft => {
            sqlx::query_scalar::<_, String>(
                "SELECT release_id FROM job_release WHERE job_id = $1 \
              ORDER BY approved_at DESC, release_id DESC LIMIT 1",
            )
            .bind(job.id)
            .fetch_optional(&mut *c)
            .await?
        }
    })
}

/// The leaf set of `(stage, kind)` in the job's design `of` (404 `kind_not_found` when the design has no such kind),
/// PK-7: the pins of `of` resolved to leaves from `release_pack_leaf` of the release (`Release(id)`; `Live` = the
/// current release, none when never released) or from the tenant's leaf cache `platform_pack_leaf` (`Draft`, then
/// the job's newest release for a pin the cache has no leaves of). A pin the cache marks unreadable or excluded
/// (`platform_pack.unreadable_at` / `excluded_by`) is `unreadable` (left out up front); a draft pin without leaves
/// that the cache knows is unsearchable (also classified missing / draft) too; any other pin without leaves is
/// `unresolved`.
pub async fn leaf_set(
    c: &mut PgConnection,
    reg: &Registry,
    job: &JobRef,
    stage: &str,
    kind: &str,
    of: &DesignOf,
) -> ApiResult<LeafSet> {
    let r = kind_ref(stage, kind);
    let Some(v) = design::value(&mut *c, reg, job, of, KIND, &r).await? else {
        return Err(kind_not_found(stage, kind));
    };
    let (store, packs) = value_parts(&v)?;
    let tenant_store = match store {
        Some(path) => {
            let all = stores::all(&mut *c).await?;
            leaf_by_path(&all, &path).map(|s| s.id)
        }
        None => None,
    };
    // T4a: the strata store of the job's pinned generation in `of` (what readiness and analytics read too)
    let tenant = match tenant_store {
        Some(id) => generations::strata_for(&mut *c, job, id, of).await?,
        None => None,
    };
    let mut out = LeafSet {
        tenant,
        tenant_store,
        ..LeafSet::default()
    };
    if packs.is_empty() {
        return Ok(out);
    }
    let cache: BTreeMap<String, CachedPack> = cached_packs(&mut *c)
        .await?
        .into_iter()
        .map(|p| (p.pin(), p))
        .collect();
    let release = leaf_release(&mut *c, job, of).await?;
    let mut leaves = match of {
        DesignOf::Draft => cached_leaves(&mut *c, &packs).await?,
        DesignOf::Live | DesignOf::Release(_) => BTreeMap::new(),
    };
    let missing: Vec<String> = packs
        .iter()
        .filter(|p| !leaves.contains_key(*p))
        .cloned()
        .collect();
    if let Some(rel) = release.as_deref()
        && !missing.is_empty()
    {
        leaves.extend(release_leaves(&mut *c, job.id, rel, &missing).await?);
    }
    for pin in packs {
        let cached = cache.get(&pin);
        let marked = cached.is_some_and(|p| p.unreadable_at.is_some() || p.excluded_by.is_some());
        match leaves.remove(&pin) {
            Some(l) => out.packs.push(PackLeaves {
                pin,
                leaves: l,
                unreadable: marked,
            }),
            None if *of == DesignOf::Draft && cached.is_some_and(CachedPack::unusable) => {
                out.packs.push(PackLeaves {
                    pin,
                    leaves: Vec::new(),
                    unreadable: true,
                });
            }
            None => out.unresolved.push(pin),
        }
    }
    Ok(out)
}

/// PK-8: the tenant's `supersedes` topic keys of the tenant leaf `store` (agent store id): the union over the
/// enabled sources writing into it and its direct uploads, sorted and deduplicated.
pub async fn tenant_supersedes(c: &mut PgConnection, store: Uuid) -> ApiResult<Vec<String>> {
    Ok(sqlx::query_scalar(
        "SELECT DISTINCT k FROM ( \
             SELECT unnest(supersedes) AS k FROM source WHERE enabled AND target_store_id = $1 \
             UNION ALL \
             SELECT unnest(supersedes) FROM knowledge_upload WHERE store_id = $1) s \
          ORDER BY k",
    )
    .bind(store)
    .fetch_all(&mut *c)
    .await?)
}

/// PK-8: the request's `supersedes` with the tenant's added, sorted and deduplicated. `None` when both are empty
/// (nothing is sent). Fails loud, never truncates: a request key that is not a topic key
/// (crate::knowledge::topic_keys) is 422 `invalid_supersedes`, and a merged set over strata's 32 keys
/// (`MAX_SUPERSEDES`) is 422 `supersedes_limit` (dropping keys would silently drop tenant overrides).
pub fn merge_supersedes(
    req: Option<&[String]>,
    tenant: &[String],
) -> ApiResult<Option<Vec<String>>> {
    let req = check_request_supersedes(req)?;
    let mut all: Vec<String> = req
        .into_iter()
        .chain(tenant.iter().map(|k| k.trim().to_string()))
        .filter(|k| !k.is_empty())
        .collect();
    all.sort();
    all.dedup();
    if all.len() > super::sources::MAX_SUPERSEDES {
        return Err(ApiError::Unprocessable(
            "supersedes_limit",
            format!(
                "The request's supersedes and this kind's tenant supersedes together name {} topic keys; at most {} \
                 can be sent. Supersede fewer topics in the request or in this kind's sources and uploads.",
                all.len(),
                super::sources::MAX_SUPERSEDES
            ),
        ));
    }
    Ok((!all.is_empty()).then_some(all))
}

/// The request's own `supersedes` through the shared validator (crate::knowledge::topic_keys): trimmed, sorted,
/// deduplicated; blank entries are ignored (they were never sent). 422 `invalid_supersedes` naming the rule.
pub fn check_request_supersedes(req: Option<&[String]>) -> ApiResult<Vec<String>> {
    let keys = req
        .unwrap_or_default()
        .iter()
        .map(|k| k.as_str())
        .filter(|k| !k.trim().is_empty());
    super::topic_keys::normalize("supersedes", keys)
        .map_err(|m| ApiError::Unprocessable("invalid_supersedes", m))
}

/// The leaf set and the tenant supersedes of `(stage, kind)`, read in one short scoped transaction (PK-8: the same
/// transaction as the leaf set), committed before returning.
async fn read_set(
    st: &AppState,
    tenant: Uuid,
    job: &JobRef,
    stage: &str,
    kind: &str,
    of: &DesignOf,
) -> ApiResult<(LeafSet, Vec<String>)> {
    let mut tx = begin_scoped(&st.db, tenant).await?;
    let set = leaf_set(&mut tx, &st.design, job, stage, kind, of).await?;
    let sup = match set.tenant_store {
        Some(s) => tenant_supersedes(&mut tx, s).await?,
        None => Vec::new(),
    };
    tx.commit().await?;
    Ok((set, sup))
}

/// The kind-scoped search (B4's entry point, R10/R11, PK-7/PK-8): the leaf set of `(stage, kind)` in `of`, searched
/// with `req` as `leaf_ids` (never `root_id` / `root_ids`), its `supersedes` widened by the tenant's (PK-8). In the
/// draft, pins the cache has no leaves of are refreshed from strata once ([`resolve::refresh_pins`], no transaction
/// open; a failure is logged and the pins degrade). An empty set makes no strata call. Pinned packs strata finds
/// unreadable degrade the answer (never fail it, however many): each pin is marked in the cache (also when the search
/// then fails for another reason), counted (`agent_knowledge_pack_unavailable_total{pack}`) and listed in
/// `degraded`, as are pins without leaves. Only the tenant leaf being unreadable (or any other strata error) fails
/// the search. No transaction is held across a strata call.
pub async fn search(
    st: &AppState,
    tenant: Uuid,
    job: &JobRef,
    stage: &str,
    kind: &str,
    of: &DesignOf,
    req: &QueryReq,
) -> ApiResult<LeafSetAnswer> {
    // refused before any read or strata call: a bad key would only become strata's 400
    check_request_supersedes(req.supersedes.as_deref())?;
    let (mut set, mut sup) = read_set(st, tenant, job, stage, kind, of).await?;
    // a draft pin the cache is cold for and the refresh could not warm: searched without, named in `cold`
    let mut cold_draft = false;
    if *of == DesignOf::Draft && !set.unresolved.is_empty() {
        match resolve::refresh_pins(st, tenant, &set.unresolved).await {
            Ok(()) => {
                let asked = set.unresolved.clone();
                (set, sup) = read_set(st, tenant, job, stage, kind, of).await?;
                // still nothing to search after a good refresh: strata cannot read it; skip the refresh next time
                let gone: Vec<String> = set
                    .unresolved
                    .iter()
                    .chain(set.packs.iter().filter(|p| !p.live()).map(|p| &p.pin))
                    .filter(|p| asked.contains(*p))
                    .cloned()
                    .collect();
                if !gone.is_empty()
                    && let Err(e) = mark_fresh(st, tenant, &gone).await
                {
                    tracing::error!(tenant = %tenant, pins = ?gone, error = %e, "knowledge_pack_unreadable_mark_failed");
                }
            }
            Err(e) => {
                tracing::warn!(tenant = %tenant, pins = ?set.unresolved, code = e.code(),
                               "knowledge_pack_refresh_failed; searching without them");
                cold_draft = true;
            }
        }
    }
    if set.is_empty() {
        let mut degraded: Vec<String> = set.packs.into_iter().map(|p| p.pin).collect();
        let mut cold = Vec::new();
        if cold_draft {
            cold = set.unresolved;
        } else {
            degraded.extend(set.unresolved);
        }
        return Ok(LeafSetAnswer {
            degraded,
            cold,
            ..LeafSetAnswer::default()
        });
    }
    let mut tx = begin_scoped(&st.db, tenant).await?;
    let p = strata::gate(st, &mut tx, tenant, Service::Rag).await?;
    tx.commit().await?;
    let mut req = req.clone();
    req.supersedes = merge_supersedes(req.supersedes.as_deref(), &sup)?;
    let run = stores::query_leaf_set(&st.http, &p, set.tenant, &set.packs, &req).await;
    // marked on every path, the failed one included, so the next search leaves them out up front
    let fresh: Vec<String> = set
        .packs
        .iter()
        .filter(|k| k.live() && run.degraded.contains(&k.pin))
        .map(|k| k.pin.clone())
        .collect();
    if !fresh.is_empty()
        && let Err(e) = mark_fresh(st, tenant, &fresh).await
    {
        // the cache only saves the next search a failing first try; the answer stands
        tracing::error!(tenant = %tenant, pins = ?fresh, error = %e, "knowledge_pack_unreadable_mark_failed");
    }
    let mut answer = run.into_answer()?;
    answer.searched = set
        .packs
        .iter()
        .filter(|k| k.live() && !answer.degraded.contains(&k.pin))
        .map(|k| k.pin.clone())
        .collect();
    if cold_draft {
        answer.cold = set.unresolved;
    } else {
        answer.degraded.extend(set.unresolved);
    }
    Ok(answer)
}

async fn mark_fresh(st: &AppState, tenant: Uuid, pins: &[String]) -> ApiResult<()> {
    let mut tx = begin_scoped(&st.db, tenant).await?;
    mark_unreadable(&mut tx, pins).await?;
    tx.commit().await?;
    Ok(())
}

/// The platform packs the tenant may pin now (strata_rag `GET /api/platform/packs`; platform knowledge off = `[]`),
/// written to the cache. No transaction is held across the call.
pub async fn refresh_packs(st: &AppState, tenant: Uuid) -> ApiResult<Vec<PlatformPack>> {
    let mut tx = begin_scoped(&st.db, tenant).await?;
    let p = strata::gate(st, &mut tx, tenant, Service::Rag).await?;
    tx.commit().await?;
    let listing = rag::platform_packs(&st.http, &p).await?;
    let mut tx = begin_scoped(&st.db, tenant).await?;
    record_listing(&mut tx, tenant, &listing).await?;
    tx.commit().await?;
    Ok(listing)
}

// ---------------------------------------------------------------------------------------------------------------
// Boot backfill (review I4)

/// The advisory lock that serialises replicas booting at once (the second finds nothing left to do).
const BACKFILL_LOCK: &str = "agent.knowledge.kinds_backfill";
/// The longest a tenant's backfill waits for a lock (the advisory lock, a job row a serving replica holds).
const BACKFILL_LOCK_TIMEOUT: &str = "5s";

/// What [`backfill`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Backfilled {
    /// Jobs that got kinds.
    pub jobs: usize,
    /// Kinds written.
    pub kinds: usize,
    /// Tenants that failed (logged; the next boot retries them).
    pub failed_tenants: usize,
}

/// A job [`backfill`] looks at: one with no knowledge kind yet.
#[derive(sqlx::FromRow)]
struct BareJob {
    id: Uuid,
    key: String,
    pins: Value,
    template_layer: Option<String>,
    template_version: Option<i32>,
    selection: Option<Value>,
}

/// Jobs created before B3 have no `job_knowledge_kind` rows (kinds are materialised at job creation and on adopted
/// pins only): every job of every tenant that has none gets its template's kinds ([`materialise_from_template`] with
/// the job's pins and its stored selection, else the template's default one). Idempotent: a job with any kind row is
/// never touched, and one whose template asks for no kind stays without. Runs at boot after migrations and BEFORE
/// `design::coverage::backfill`, so the rows it writes are what that backfill copies into the job's older baseline
/// and releases (no phantom draft change). Per tenant: one RLS-scoped transaction under an advisory lock, every lock
/// wait bounded by `lock_timeout`; a failing tenant is logged (`knowledge_kinds_backfill_failed`), counted and
/// skipped. Never fails the boot.
pub async fn backfill(db: &sqlx::PgPool) -> Backfilled {
    let mut out = Backfilled::default();
    let tenants = match list_tenants(db).await {
        Ok(t) => t,
        Err(e) => {
            tracing::error!(error = %e, "knowledge_kinds_backfill_tenants_unreadable");
            return out;
        }
    };
    for tenant in tenants {
        match backfill_tenant(db, tenant).await {
            Ok((jobs, kinds)) => {
                out.jobs += jobs;
                out.kinds += kinds;
            }
            Err(e) => {
                tracing::error!(tenant = %tenant, error = %e, "knowledge_kinds_backfill_failed");
                out.failed_tenants += 1;
            }
        }
    }
    tracing::info!(
        jobs = out.jobs,
        kinds = out.kinds,
        failed_tenants = out.failed_tenants,
        "knowledge_kinds_backfill"
    );
    out
}

/// Every tenant (`agent_list_tenants()` is granted to `agent_platform` alone, 0031).
async fn list_tenants(db: &sqlx::PgPool) -> ApiResult<Vec<Uuid>> {
    let mut tx = crate::platform_db::begin_platform(db).await?;
    let tenants: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM agent_list_tenants()")
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(tenants)
}

/// One tenant: `(jobs that got kinds, kinds written)`. Rolled back (on drop) when anything fails.
async fn backfill_tenant(db: &sqlx::PgPool, tenant: Uuid) -> ApiResult<(usize, usize)> {
    let mut tx = begin_scoped(db, tenant).await?;
    sqlx::query("SELECT set_config('lock_timeout', $1, true)")
        .bind(BACKFILL_LOCK_TIMEOUT)
        .execute(&mut *tx)
        .await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1))")
        .bind(BACKFILL_LOCK)
        .execute(&mut *tx)
        .await?;
    // the job rows locked as `design::track` locks them, so a tracked write on a serving replica cannot interleave
    let jobs: Vec<BareJob> = sqlx::query_as(
        "SELECT j.id, j.key, j.pins, j.template_layer, j.template_version, t.selection \
           FROM jobs j LEFT JOIN job_target t ON t.job_id = j.id \
          WHERE NOT EXISTS (SELECT 1 FROM job_knowledge_kind k WHERE k.job_id = j.id) \
          ORDER BY j.id FOR UPDATE OF j",
    )
    .fetch_all(&mut *tx)
    .await?;
    let (mut done, mut written) = (0, 0);
    for j in jobs {
        let pins: Vec<TemplatePin> = serde_json::from_value(j.pins)
            .map_err(|e| internal(format!("jobs.pins of {} is unreadable: {e}", j.key)))?;
        let sel: Option<Selection> = match j.selection {
            Some(v) => Some(serde_json::from_value(v).map_err(|e| {
                internal(format!(
                    "job_target.selection of {} is unreadable: {e}",
                    j.key
                ))
            })?),
            None => match (j.template_layer.as_deref(), j.template_version) {
                (Some(layer), Some(version)) => {
                    design::default_selection(&mut tx, layer, version).await?
                }
                _ => None,
            },
        };
        let job = JobRef {
            id: j.id,
            key: j.key,
        };
        let seeds = template_seeds(&mut tx, &pins, sel.as_ref(), Some(&job)).await?;
        if seeds.is_empty() {
            continue;
        }
        materialise(&mut tx, &job, &seeds).await?;
        done += 1;
        written += seeds.len();
    }
    tx.commit().await?;
    Ok((done, written))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pins_follow_the_check() {
        for ok in [
            "base@1",
            "industry/retail@2",
            "a-b_c/d@999999999",
            "a/b/c/d/e/f@3",
        ] {
            assert!(parse_pin(ok).is_some(), "{ok}");
        }
        for bad in [
            "base",
            "base@0",
            "base@01",
            "base@1000000000",
            "Base@1",
            "@1",
            "a//b@1",
            "a/b/c/d/e/f/g@1",
            "base@x",
            "base@",
            "ba se@1",
        ] {
            assert!(parse_pin(bad).is_none(), "{bad}");
        }
        assert_eq!(
            parse_pin("industry/retail@2"),
            Some(PackPin {
                key: "industry/retail".into(),
                version: 2
            })
        );
    }

    #[test]
    fn check_pins_sorts_and_refuses() {
        let got = check_pins(&["z@1".to_string(), "base@2".to_string()]).unwrap();
        assert_eq!(
            got.iter().map(PackPin::pin).collect::<Vec<_>>(),
            ["base@2", "z@1"]
        );
        assert_eq!(
            check_pins(&["base@1".to_string(), "base@2".to_string()])
                .unwrap_err()
                .code(),
            "invalid_pack"
        );
        let many: Vec<String> = (0..17).map(|i| format!("p{i}@1")).collect();
        assert_eq!(check_pins(&many).unwrap_err().code(), "invalid_pack");
        assert!(check_pins(&many[..16]).is_ok());
    }

    #[test]
    fn value_text_names_store_and_pins() {
        assert_eq!(value_text(&None), "unbound");
        assert_eq!(value_text(&Some(kind_value(None, &[]))), "unbound");
        assert_eq!(
            value_text(&Some(kind_value(
                Some("faq/policy"),
                &["base@1".to_string(), "industry/retail@2".to_string()]
            ))),
            "faq/policy + base@1 + industry/retail@2"
        );
    }

    #[test]
    fn seed_parts_follow_the_0054_checks() {
        assert_eq!(
            seed_parts("support|tariff.sheet"),
            Some(("support", "tariff.sheet"))
        );
        let stage64 = format!("{}|faq", "s".repeat(64));
        assert!(seed_parts(&stage64).is_some());
        let kind128 = format!("support|{}", "k".repeat(128));
        assert!(seed_parts(&kind128).is_some());
        let stage65 = format!("{}|faq", "s".repeat(65));
        let kind129 = format!("support|{}", "k".repeat(129));
        for bad in [
            "support",
            "|faq",
            "support|",
            "support:returns|faq",
            "a|b|c",
            "support|faq:x",
            "sup port|faq",
            "support|faq\tx",
            stage65.as_str(),
            kind129.as_str(),
        ] {
            assert_eq!(seed_parts(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn newer_is_the_highest_listed_version_above_the_pin() {
        let listed = [
            ("base".to_string(), 1),
            ("base".to_string(), 3),
            ("base".to_string(), 2),
            ("other".to_string(), 9),
        ];
        assert_eq!(newer_of("base@1", &listed).as_deref(), Some("base@3"));
        assert_eq!(newer_of("base@3", &listed), None);
        assert_eq!(newer_of("gone@1", &listed), None);
    }

    #[test]
    fn supersedes_merge_is_sorted_deduplicated_and_fails_loud() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<String>>();
        let code = |r: ApiResult<Option<Vec<String>>>| r.unwrap_err().code().to_string();
        assert_eq!(
            merge_supersedes(None, &[]).unwrap(),
            None,
            "nothing to send"
        );
        assert_eq!(
            merge_supersedes(Some(&s(&[])[..]), &s(&[" "])).unwrap(),
            None
        );
        assert_eq!(
            merge_supersedes(
                Some(&s(&["hr/leave", "faq/returns"])[..]),
                &s(&["faq/returns", "hr/a"])
            )
            .unwrap(),
            Some(s(&["faq/returns", "hr/a", "hr/leave"]))
        );
        assert_eq!(
            merge_supersedes(None, &s(&["x"])).unwrap(),
            Some(s(&["x"])),
            "the tenant's keys alone are sent"
        );
        let many: Vec<String> = (0..40).map(|i| format!("k{i:02}")).collect();
        // exactly 32 merged keys pass; 33 fail loud, nothing is dropped
        assert_eq!(
            merge_supersedes(Some(&many[..16]), &many[16..32])
                .unwrap()
                .map(|v| v.len()),
            Some(32)
        );
        assert_eq!(
            code(merge_supersedes(Some(&many[..20]), &many[20..])),
            "supersedes_limit"
        );
        assert_eq!(
            code(merge_supersedes(Some(&many[..1]), &many[..33])),
            "supersedes_limit",
            "the tenant's set alone over the limit is refused, not truncated"
        );
        // the request's keys go through the shared topic-key validator
        for bad in ["Hours", "a b", "hr:leave"] {
            assert_eq!(
                code(merge_supersedes(Some(&s(&[bad])[..]), &[])),
                "invalid_supersedes",
                "{bad:?}"
            );
        }
        assert_eq!(
            code(merge_supersedes(Some(&many[..33]), &[])),
            "invalid_supersedes",
            "the request alone past 32 keys"
        );
    }
}
