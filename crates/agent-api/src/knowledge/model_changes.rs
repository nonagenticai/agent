//! Model changes (B3 T4a; D83, D92, D93; plan §5.8): a tenant slot's model moves, every job pinning the old model
//! gets a pending change, and each job rebuilds and publishes on its own.
//!
//! - [`on_slot_saved`] (called by `saveProviderSlot` / reset in its write transaction): one `model_change` (scope
//!   `tenant`, the slot's `rebuild`, crate::knowledge::generations::rebuild_of) and one `model_change_job` per job whose
//!   DRAFT pin of some root store runs another model than the new one; older open changes of the slot are dismissed.
//!   Jobs already on the new model get no row. Nothing reaches strata here.
//! - [`reembed`] (`reembedJobStore`, a person's click — never a timer, no background loop): per affected store, reuse
//!   the `building` / `ready` generation that already runs the target model set (another job's), else create one: a
//!   NEW strata root (R7), its store bindings (rag embed/rerank/chat, ingest ocr/vision/transcribe: the tenant's
//!   working slot registrations whose model is in the set, keys opened here and sent only to strata), then the
//!   rebuild — rag `reembed-copy` (`reembed`), rag `reembed-copy {copy_vectors: true}` (`copy`) or ingest
//!   `reingest-copy` (`reingest`). No transaction is held across a strata call.
//! - [`refresh`] (on every read and before an accept): a generation is `ready` only when strata reports the job
//!   `done` — which includes its delta passes (D92); `failed` (e.g. `conflict`, did not converge) fails the
//!   generation and puts the change back to `pending`. A finished copy's subtree is mapped store by store (strata
//!   names = agent keys). strata_ingest's `missing_originals` are kept on the generation and shown.
//! - [`accept`] (`acceptModelChange`): every target generation `ready` → `design::track` writes the job's
//!   `embedding_pin` draft (one per store); 409 `generation_not_ready` + `stores` otherwise, 409
//!   `originals_not_retained` + `documents` while a reingest left documents behind (unless the caller acknowledges
//!   them: they must be re-uploaded or re-synced).
//! - [`on_published`] (the kind's `published` hook): the change is `published` for the job once every store's live pin
//!   is its target generation; generations nothing uses any more are retired; an embed push withheld for roots on
//!   the deployment default is queued again (crate::providers::slots::requeue_withheld; also on accept).
//! - [`sweep`] (boot + the outbox drainer's tick, never a rebuild): stranded `building` generations fail back to
//!   `pending`, [`catch_up`] fans changes out to jobs and roots that appeared later, a withheld embed push is retried.
use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use super::generations::{self, Generation, ModelSet};
use super::stores::{self, StoreRow};
use super::targets;
use crate::audit::{self, Actor, Event};
use crate::design::{self, DesignOf, DraftWrite, JobRef, Label, Registry};
use crate::error::{ApiError, ApiResult};
use crate::providers::kinds::Slot;
use crate::providers::secret::{self, SecretKind, SecretText};
use crate::providers::slots::PUBLISHER;
use crate::provision::senders::store_delete;
use crate::state::AppState;
use crate::strata::rag::{self, NewStore, StoreKind};
use crate::strata::{self, Plane, Service, StrataStoreId, ingest};
use crate::tenancy::{self, JobCtx, begin_scoped};

/// `model_change_job.state` values (the `modelChanges?state=` filter).
pub const STATES: [&str; 6] = [
    "pending",
    "reembedding",
    "ready",
    "accepted",
    "published",
    "dismissed",
];
/// The actor of the sweeper and of [`catch_up`].
pub const SWEEPER: &str = "service:agent-api-model-changes";
/// The ledger area and owning page of a pin write.
const AREA: &str = "knowledge";
const PAGE: &str = "T7";

fn internal(what: String) -> ApiError {
    ApiError::Internal(anyhow::anyhow!(what))
}

/// 404 `model_change_not_found` (no such change, or the job has no part in it).
pub fn not_found() -> ApiError {
    ApiError::Missing(
        "model_change_not_found",
        "No such model change for this job.".into(),
    )
}

fn already_reembedding() -> ApiError {
    ApiError::Conflict(
        "already_reembedding",
        "This job's stores are already being rebuilt for this change.".into(),
    )
}

/// 409 `change_superseded`: a newer save of the slot dismissed this change (review I1 / M9).
fn superseded() -> ApiError {
    ApiError::Conflict(
        "change_superseded",
        "A newer change of this provider slot replaced this one; rebuild and accept that one."
            .into(),
    )
}

fn already_accepted() -> ApiError {
    ApiError::Conflict(
        "already_accepted",
        "This change is already in the job's draft.".into(),
    )
}

/// One affected root store of a job's change.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoreEntry {
    pub store_id: Uuid,
    pub from_generation: Uuid,
    #[serde(default)]
    pub to_generation: Option<Uuid>,
}

fn entries_of(v: &Value) -> ApiResult<Vec<StoreEntry>> {
    serde_json::from_value(v.clone()).map_err(|e| internal(format!("model_change_job.stores: {e}")))
}

fn entries_json(e: &[StoreEntry]) -> ApiResult<Value> {
    serde_json::to_value(e).map_err(|e| internal(format!("model_change_job.stores: {e}")))
}

/// One `model_change` row.
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Change {
    pub id: Uuid,
    pub scope: String,
    pub slot: String,
    pub from_model: Option<String>,
    pub to_model: Option<String>,
    pub rebuild: String,
    pub created_at: DateTime<Utc>,
    pub created_by: String,
}

impl Change {
    pub fn slot(&self) -> ApiResult<Slot> {
        Slot::parse(&self.slot).ok_or_else(|| internal(format!("model_change slot {}", self.slot)))
    }

    fn json(&self) -> Value {
        json!({
            "id": self.id,
            "scope": self.scope,
            "slot": self.slot,
            "from_model": self.from_model,
            "to_model": self.to_model,
            "rebuild": self.rebuild,
            "created_at": self.created_at,
            "created_by": self.created_by,
        })
    }
}

/// One `model_change_job` row with its job's key, draft revision and live release.
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct ChangeJob {
    pub model_change_id: Uuid,
    pub job_id: Uuid,
    pub job_key: String,
    pub state: String,
    pub stores: Value,
    pub chunks: Option<i32>,
    pub updated_at: DateTime<Utc>,
    pub draft_rev: i64,
    pub live_release: Option<String>,
}

const CHANGE_COLS: &str = "id, scope, slot, from_model, to_model, rebuild, created_at, created_by";
const JOB_SELECT: &str = "SELECT m.model_change_id, m.job_id, j.key AS job_key, m.state, m.stores, m.chunks, \
                                 m.updated_at, j.draft_rev, j.current_release_id AS live_release \
                            FROM model_change_job m JOIN jobs j ON j.id = m.job_id";

async fn change(c: &mut PgConnection, id: Uuid) -> ApiResult<Option<Change>> {
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {CHANGE_COLS} FROM model_change WHERE id = $1"
    )))
    .bind(id)
    .fetch_optional(&mut *c)
    .await?)
}

async fn change_jobs(c: &mut PgConnection, id: Uuid) -> ApiResult<Vec<ChangeJob>> {
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{JOB_SELECT} WHERE m.model_change_id = $1 ORDER BY j.key"
    )))
    .bind(id)
    .fetch_all(&mut *c)
    .await?)
}

async fn change_job(
    c: &mut PgConnection,
    id: Uuid,
    job: Uuid,
    lock: bool,
) -> ApiResult<Option<ChangeJob>> {
    let lock = if lock { " FOR UPDATE OF m" } else { "" };
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{JOB_SELECT} WHERE m.model_change_id = $1 AND m.job_id = $2{lock}"
    )))
    .bind(id)
    .bind(job)
    .fetch_optional(&mut *c)
    .await?)
}

async fn set_job_state(
    c: &mut PgConnection,
    id: Uuid,
    job: Uuid,
    state: &str,
    entries: &[StoreEntry],
    by: &str,
) -> ApiResult<()> {
    sqlx::query(
        "UPDATE model_change_job SET state = $3, stores = $4, updated_by = $5, updated_at = clock_timestamp() \
          WHERE model_change_id = $1 AND job_id = $2",
    )
    .bind(id)
    .bind(job)
    .bind(state)
    .bind(entries_json(entries)?)
    .bind(by)
    .execute(&mut *c)
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------------------------
// Fan-out (saveProviderSlot)

/// D83/D93: `slot`'s working model of the scoped tenant moves from `from` to `to` (`None` = the deployment default).
/// Records the change and fans it out to every job whose draft pin of a root store runs anything but `to` (review I1:
/// the jobs' generations, not the working row, say what they run — a second save before a publish reaches the jobs
/// still on the published model). The open (`pending` / `reembedding` / `ready`) parts of older changes of the same
/// slot are `dismissed`: the newer change supersedes them. The change's id, `None` when the working model does not
/// move. Call it in the save's transaction.
pub async fn on_slot_saved(
    c: &mut PgConnection,
    tenant: Uuid,
    slot: Slot,
    from: Option<&str>,
    to: Option<&str>,
    by: &str,
) -> ApiResult<Option<Uuid>> {
    if from == to {
        return Ok(None);
    }
    generations::ensure_initial_all(&mut *c, tenant, by).await?;
    sqlx::query(
        "UPDATE model_change_job m SET state = 'dismissed', updated_by = $2, updated_at = clock_timestamp() \
           FROM model_change ch \
          WHERE ch.id = m.model_change_id AND ch.slot = $1 \
            AND m.state IN ('pending', 'reembedding', 'ready')",
    )
    .bind(slot.as_str())
    .bind(by)
    .execute(&mut *c)
    .await?;
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO model_change (id, scope, slot, from_model, to_model, rebuild, created_by) \
         VALUES ($1, 'tenant', $2, $3, $4, $5, $6)",
    )
    .bind(id)
    .bind(slot.as_str())
    .bind(from)
    .bind(to)
    .bind(generations::rebuild_of(slot))
    .bind(by)
    .execute(&mut *c)
    .await?;
    let roots = root_stores(&mut *c).await?;
    let jobs: Vec<(Uuid, String)> = sqlx::query_as("SELECT id, key FROM jobs ORDER BY id")
        .fetch_all(&mut *c)
        .await?;
    for (job_id, key) in jobs {
        let jr = JobRef { id: job_id, key };
        let entries = off_target(&mut *c, &jr, &roots, slot, to, &[]).await?;
        if entries.is_empty() {
            continue;
        }
        sqlx::query(
            "INSERT INTO model_change_job (model_change_id, job_id, state, stores, updated_by) \
             VALUES ($1, $2, 'pending', $3, $4)",
        )
        .bind(id)
        .bind(job_id)
        .bind(entries_json(&entries)?)
        .bind(by)
        .execute(&mut *c)
        .await?;
    }
    Ok(Some(id))
}

/// The scoped tenant's root stores.
async fn root_stores(c: &mut PgConnection) -> ApiResult<Vec<StoreRow>> {
    Ok(stores::all(&mut *c)
        .await?
        .into_iter()
        .filter(|s| s.parent_id.is_none())
        .collect())
}

/// The roots (not in `skip`) whose generation `job`'s draft pins runs another `slot` model than `to`: one entry each.
async fn off_target(
    c: &mut PgConnection,
    job: &JobRef,
    roots: &[StoreRow],
    slot: Slot,
    to: Option<&str>,
    skip: &[Uuid],
) -> ApiResult<Vec<StoreEntry>> {
    let mut entries = Vec::new();
    for root in roots.iter().filter(|r| !skip.contains(&r.id)) {
        let Some(g) = generations::pinned(&mut *c, job, root, &DesignOf::Draft).await? else {
            continue;
        };
        if g.models().get(slot) != to {
            entries.push(StoreEntry {
                store_id: root.id,
                from_generation: g.id,
                to_generation: None,
            });
        }
    }
    Ok(entries)
}

/// The fan-out [`on_slot_saved`] would make now, for what appeared after it (review I3): a job created later (it
/// reads the stores' `initial` generations) or a root store created later (its `initial` runs what strata ran then,
/// e.g. the deployment default while an embed push is withheld). Per slot, the newest change still names the working
/// model: every job whose draft pin of a root runs another model and that has no entry for that root in it gets one
/// (its part `pending` again when it was `ready` / `accepted` / `published`; a part being rebuilt or dismissed is
/// left alone). The number of job parts written. Runs on every model-change read (crate::knowledge::model_changes
/// `refresh`) and every sweep.
pub async fn catch_up(c: &mut PgConnection, tenant: Uuid, by: &str) -> ApiResult<usize> {
    let latest: Vec<Change> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT DISTINCT ON (slot) {CHANGE_COLS} FROM model_change ORDER BY slot, created_at DESC, id DESC"
    )))
    .fetch_all(&mut *c)
    .await?;
    if latest.is_empty() {
        return Ok(0);
    }
    let (working, _) = generations::tenant_models(&mut *c, tenant).await?;
    let roots = root_stores(&mut *c).await?;
    let jobs: Vec<(Uuid, String)> = sqlx::query_as("SELECT id, key FROM jobs ORDER BY id")
        .fetch_all(&mut *c)
        .await?;
    let mut written = 0;
    for ch in &latest {
        let slot = ch.slot()?;
        let to = ch.to_model.as_deref();
        if working.get(slot) != to {
            continue;
        }
        for (job_id, key) in &jobs {
            let part: Option<(String, Value)> = sqlx::query_as(
                "SELECT state, stores FROM model_change_job WHERE model_change_id = $1 AND job_id = $2 FOR UPDATE",
            )
            .bind(ch.id)
            .bind(job_id)
            .fetch_optional(&mut *c)
            .await?;
            if part
                .as_ref()
                .is_some_and(|(s, _)| s == "reembedding" || s == "dismissed")
            {
                continue;
            }
            let mut entries = match &part {
                Some((_, v)) => entries_of(v)?,
                None => Vec::new(),
            };
            let have: Vec<Uuid> = entries.iter().map(|e| e.store_id).collect();
            let jr = JobRef {
                id: *job_id,
                key: key.clone(),
            };
            let new = off_target(&mut *c, &jr, &roots, slot, to, &have).await?;
            if new.is_empty() {
                continue;
            }
            entries.extend(new);
            let q = if part.is_some() {
                "UPDATE model_change_job SET state = 'pending', stores = $3, updated_by = $4, \
                        updated_at = clock_timestamp() \
                  WHERE model_change_id = $1 AND job_id = $2"
            } else {
                "INSERT INTO model_change_job (model_change_id, job_id, state, stores, updated_by) \
                 VALUES ($1, $2, 'pending', $3, $4) ON CONFLICT DO NOTHING"
            };
            sqlx::query(q)
                .bind(ch.id)
                .bind(job_id)
                .bind(entries_json(&entries)?)
                .bind(by)
                .execute(&mut *c)
                .await?;
            written += 1;
        }
    }
    Ok(written)
}

// ---------------------------------------------------------------------------------------------------------------
// Rebuild (reembedJobStore)

/// One configured slot's working registration, its key opened (never logged; no `Debug`).
struct Binding {
    slot: Slot,
    kind: String,
    base_url: String,
    model: String,
    key: Option<SecretText>,
}

#[derive(sqlx::FromRow)]
struct BindingRow {
    slot: String,
    kind: Option<String>,
    base_url: Option<String>,
    model: Option<String>,
    secret_id: Option<Uuid>,
    key_sealed: Option<String>,
}

/// The tenant's configured slots with an endpoint, keys opened.
async fn bindings(c: &mut PgConnection, st: &AppState, tenant: Uuid) -> ApiResult<Vec<Binding>> {
    let rows: Vec<BindingRow> = sqlx::query_as(
        "SELECT s.slot, s.kind, s.base_url, s.model, s.secret_id, k.key_sealed \
           FROM provider_slot s \
           LEFT JOIN provider_slot_secret k ON k.tenant_id = s.tenant_id AND k.id = s.secret_id \
          WHERE s.tenant_id = $1 AND s.configured",
    )
    .bind(tenant)
    .fetch_all(&mut *c)
    .await?;
    let mut out = Vec::new();
    for r in rows {
        let (Some(slot), Some(kind), Some(base_url), Some(model)) =
            (Slot::parse(&r.slot), r.kind, r.base_url, r.model)
        else {
            continue;
        };
        let key = match (r.secret_id, r.key_sealed) {
            (Some(id), Some(sealed)) => Some(secret::open(
                st.provider_keyring.as_deref(),
                tenant,
                SecretKind::SlotKey,
                id,
                &sealed,
            )?),
            _ => None,
        };
        out.push(Binding {
            slot,
            kind,
            base_url,
            model,
            key,
        });
    }
    Ok(out)
}

/// The store-binding PUT body of `service` for a generation running `target`: every working registration whose model
/// is the set's (a capability left out inherits); `None` when there is none.
fn binding_body(
    bs: &[Binding],
    service: Service,
    target: &ModelSet,
    residency: &str,
) -> Option<Value> {
    let mut m = Map::new();
    for b in bs {
        if b.slot.service() != service || target.get(b.slot) != Some(b.model.as_str()) {
            continue;
        }
        let dialect = b
            .slot
            .dialect(&b.kind)
            .map_or_else(|| b.kind.clone(), str::to_string);
        let mut e = Map::new();
        e.insert("base_url".into(), json!(b.base_url));
        e.insert("dialect".into(), json!(dialect));
        e.insert("model".into(), json!(b.model));
        e.insert("regions".into(), json!([residency]));
        e.insert("zdr".into(), json!(true));
        if let Some(d) = b.slot.dims() {
            e.insert("dims".into(), json!(d));
        }
        if let Some(k) = &b.key {
            e.insert("api_key".into(), json!(k.expose()));
        }
        m.insert(b.slot.as_str().to_string(), Value::Object(e));
    }
    (!m.is_empty()).then_some(Value::Object(m))
}

/// The `provider_ref` of a generation running `target`: the working slot's reference where the model is the working
/// one, `inherited` where the set keeps an older model (no registration of it is held any more).
fn provider_ref(target: &ModelSet, working: &ModelSet, refs: &Value) -> Value {
    let mut m = Map::new();
    for slot in Slot::ALL {
        let v = if target.get(slot) == working.get(slot) {
            refs.get(slot.as_str()).cloned().unwrap_or(Value::Null)
        } else {
            json!({"source": "inherited"})
        };
        m.insert(slot.as_str().to_string(), v);
    }
    Value::Object(m)
}

/// The strata name of generation `generation`'s root of agent root store `key`: `<key>.gen-<first 8 hex>`.
fn gen_root_name(key: &str, generation: Uuid) -> String {
    let short: String = generation.simple().to_string().chars().take(8).collect();
    format!("{key}.gen-{short}")
}

/// A generation to build.
struct Build {
    generation: Uuid,
    root: StoreRow,
    /// The source generation's strata root.
    source: StrataStoreId,
    target: ModelSet,
}

/// Create one generation's strata root, bind it and start its rebuild: `(root, "<source>/<job>")`. `created` is set
/// as soon as the root exists, so the caller can delete it when a later step fails.
#[allow(clippy::too_many_arguments)]
async fn run_build(
    st: &AppState,
    rag_p: &Plane<'_>,
    ing_p: Option<&Plane<'_>>,
    rebuild: &str,
    b: &Build,
    bs: &[Binding],
    residency: &str,
    created: &mut Option<StrataStoreId>,
) -> ApiResult<(StrataStoreId, String)> {
    let ingest_plane =
        || ing_p.ok_or_else(|| internal("an ingest rebuild without the ingest plane".to_string()));
    let new = NewStore {
        parent_id: None,
        kind: if b.root.kind == "group" {
            StoreKind::Router
        } else {
            StoreKind::Leaf
        },
        name: gen_root_name(&b.root.key, b.generation),
        description: b.root.label.clone(),
    };
    let sid = rag::create_store(&st.http, rag_p, &new).await?.id;
    *created = Some(sid);
    if let Some(body) = binding_body(bs, Service::Rag, &b.target, residency) {
        rag::store_providers_put(&st.http, rag_p, sid, body).await?;
    }
    if let Some(body) = binding_body(bs, Service::Ingest, &b.target, residency) {
        ingest::store_providers_put(&st.http, ingest_plane()?, sid, body).await?;
    }
    let job = if rebuild == "reingest" {
        ingest::reingest_copy_start(&st.http, ingest_plane()?, b.source, sid)
            .await?
            .job
    } else {
        rag::reembed_copy_start(&st.http, rag_p, b.source, sid, rebuild == "copy")
            .await?
            .job
    };
    Ok((sid, format!("{}/{job}", b.source)))
}

/// `reembedJobStore`: start (or reuse) the rebuilds of one job's change; the job's view of the change.
pub async fn reembed(st: &AppState, j: &JobCtx, id: Uuid) -> ApiResult<Value> {
    let tenant = j.t.tenant;
    let by = j.t.caller.sub.clone();
    // 1. claim the generations to build (committed before any strata call)
    let (ch, builds, bs, residency, entries) = {
        let mut tx = j.begin(&st.db).await?;
        tenancy::lock_tenant_row(&mut tx, tenant).await?;
        let ch = change(&mut tx, id).await?.ok_or_else(not_found)?;
        let cj = change_job(&mut tx, id, j.job_id, true)
            .await?
            .ok_or_else(not_found)?;
        match cj.state.as_str() {
            "reembedding" => return Err(already_reembedding()),
            "accepted" | "published" => return Err(already_accepted()),
            "dismissed" => return Err(superseded()),
            "ready" => {
                let v = job_view(&mut tx, &ch, &cj).await?;
                tx.commit().await?;
                return Ok(v);
            }
            _ => {}
        }
        let slot = ch.slot()?;
        let all = stores::all(&mut tx).await?;
        let (working, refs) = generations::tenant_models(&mut tx, tenant).await?;
        let mut entries = entries_of(&cj.stores)?;
        let mut builds = Vec::new();
        for e in entries.iter_mut() {
            let from = generations::by_id(&mut tx, e.from_generation)
                .await?
                .ok_or_else(|| internal(format!("generation {} is gone", e.from_generation)))?;
            let root = all
                .iter()
                .find(|s| s.id == e.store_id)
                .ok_or_else(stores::store_not_found)?;
            let target = from.models().with(slot, ch.to_model.as_deref());
            if let Some(g) = generations::with_models(&mut tx, root.id, &target).await? {
                e.to_generation = Some(g.id);
                continue;
            }
            let source = from.root_sid(root).ok_or_else(|| {
                ApiError::Conflict(
                    "provision_pending",
                    format!("{} is still being created in strata_rag.", root.key),
                )
            })?;
            let gid = Uuid::new_v4();
            let inserted = sqlx::query(
                "INSERT INTO store_generation (id, store_id, embed, rerank, chat, ocr, vision, transcribe, \
                                               provider_ref, build, state, source_id, created_by) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, 'building', $11, $12)",
            )
            .bind(gid)
            .bind(root.id)
            .bind(&target.embed)
            .bind(&target.rerank)
            .bind(&target.chat)
            .bind(&target.ocr)
            .bind(&target.vision)
            .bind(&target.transcribe)
            .bind(provider_ref(&target, &working, &refs))
            .bind(&ch.rebuild)
            .bind(from.id)
            .bind(&by)
            .execute(&mut *tx)
            .await;
            if let Err(e) = inserted {
                let dup = e
                    .as_database_error()
                    .and_then(|d| d.code())
                    .is_some_and(|c| c == "23505");
                return Err(if dup {
                    already_reembedding()
                } else {
                    ApiError::from(e)
                });
            }
            e.to_generation = Some(gid);
            builds.push(Build {
                generation: gid,
                root: root.clone(),
                source,
                target,
            });
        }
        let bs = if builds.is_empty() {
            Vec::new()
        } else {
            bindings(&mut tx, st, tenant).await?
        };
        let residency: String = sqlx::query_scalar("SELECT residency FROM tenants WHERE id = $1")
            .bind(tenant)
            .fetch_one(&mut *tx)
            .await?;
        let ids: Vec<Uuid> = entries.iter().filter_map(|e| e.to_generation).collect();
        let gens = generations::by_ids(&mut tx, &ids).await?;
        let all_ready = entries.iter().all(|e| {
            e.to_generation
                .and_then(|g| gens.get(&g))
                .is_some_and(Generation::is_ready)
        });
        let state = if all_ready { "ready" } else { "reembedding" };
        set_job_state(&mut tx, id, j.job_id, state, &entries, &by).await?;
        tx.commit().await?;
        (ch, builds, bs, residency, entries)
    };
    // 2. the strata calls (no transaction open)
    let mut started: Vec<(Uuid, StrataStoreId, String)> = Vec::new();
    let mut orphans: Vec<StrataStoreId> = Vec::new();
    let mut failure: Option<ApiError> = None;
    if !builds.is_empty() {
        let planes = async {
            let r = strata::plane(st, tenant, Service::Rag).await?;
            let needs_ingest = ch.rebuild == "reingest"
                || builds
                    .iter()
                    .any(|b| binding_body(&bs, Service::Ingest, &b.target, &residency).is_some());
            let i = if needs_ingest {
                Some(strata::plane(st, tenant, Service::Ingest).await?)
            } else {
                None
            };
            ApiResult::Ok((r.with_actor(&j.t.caller.sub), i))
        }
        .await;
        match planes {
            Err(e) => failure = Some(e),
            Ok((rag_p, ing_p)) => {
                for b in &builds {
                    let mut created = None;
                    match run_build(
                        st,
                        &rag_p,
                        ing_p.as_ref(),
                        &ch.rebuild,
                        b,
                        &bs,
                        &residency,
                        &mut created,
                    )
                    .await
                    {
                        Ok((sid, job)) => started.push((b.generation, sid, job)),
                        Err(e) => {
                            orphans.extend(created);
                            failure = Some(e);
                            break;
                        }
                    }
                }
            }
        }
    }
    // 3. record what started; undo what did not
    let mut tx = j.begin(&st.db).await?;
    for (gid, sid, job) in &started {
        let done = sqlx::query(
            "UPDATE store_generation SET strata_store_id = $2, reembed_job = $3, updated_at = clock_timestamp() \
              WHERE id = $1 AND state = 'building'",
        )
        .bind(gid)
        .bind(sid.0)
        .bind(job)
        .execute(&mut *tx)
        .await?;
        if done.rows_affected() == 0 {
            // the sweeper failed it meanwhile ([`sweep`]): the root just made is nobody's
            store_delete::enqueue(&mut tx, tenant, *sid, &j.t.actor()).await?;
        }
    }
    let ch_slot = ch.slot.clone();
    if let Some(e) = failure {
        let unstarted: Vec<Uuid> = builds
            .iter()
            .map(|b| b.generation)
            .filter(|g| !started.iter().any(|(s, _, _)| s == g))
            .collect();
        sqlx::query("DELETE FROM store_generation WHERE id = ANY($1) AND reembed_job IS NULL")
            .bind(&unstarted)
            .execute(&mut *tx)
            .await?;
        for sid in &orphans {
            store_delete::enqueue(&mut tx, tenant, *sid, &j.t.actor()).await?;
        }
        let entries: Vec<StoreEntry> = entries
            .into_iter()
            .map(|mut e| {
                if e.to_generation.is_some_and(|g| unstarted.contains(&g)) {
                    e.to_generation = None;
                }
                e
            })
            .collect();
        set_job_state(&mut tx, id, j.job_id, "pending", &entries, &by).await?;
        tx.commit().await?;
        return Err(e);
    }
    audit::record(
        &mut tx,
        &j.t.actor(),
        Event::new("model_change.reembed", "model_change", id.to_string())
            .job(&j.job_key)
            .detail("slot", ch_slot)
            .detail("rebuild", ch.rebuild.clone())
            .detail("new_generations", started.len()),
    )
    .await?;
    let cj = change_job(&mut tx, id, j.job_id, false)
        .await?
        .ok_or_else(not_found)?;
    let v = job_view(&mut tx, &ch, &cj).await?;
    tx.commit().await?;
    Ok(v)
}

// ---------------------------------------------------------------------------------------------------------------
// Refresh (D92)

/// What strata reports for one rebuild.
enum Polled {
    Running { chunks: Option<i64> },
    Done { chunks: Option<i64>, missing: Value },
    Failed(String),
}

async fn poll(st: &AppState, tenant: Uuid, g: &Generation) -> ApiResult<Option<Polled>> {
    let Some((source, job)) = g.reembed_job.as_deref().and_then(|s| s.split_once('/')) else {
        return Ok(None);
    };
    let (Ok(source), Ok(job)) = (Uuid::parse_str(source), Uuid::parse_str(job)) else {
        return Ok(None);
    };
    let source = StrataStoreId(source);
    if g.build == "reingest" {
        let p = strata::plane(st, tenant, Service::Ingest).await?;
        let v = ingest::reingest_copy_get(&st.http, &p, source, job).await?;
        return Ok(Some(match v.state.as_str() {
            "done" => Polled::Done {
                chunks: Some(v.documents_done),
                missing: serde_json::to_value(&v.missing_originals).unwrap_or_else(|_| json!([])),
            },
            "failed" => Polled::Failed(
                v.error
                    .as_ref()
                    .and_then(|e| e.get("message").or_else(|| e.get("code")))
                    .and_then(Value::as_str)
                    .unwrap_or("the re-ingest copy failed")
                    .to_string(),
            ),
            _ => Polled::Running {
                chunks: Some(v.documents_done),
            },
        }));
    }
    let p = strata::plane(st, tenant, Service::Rag).await?;
    let v = rag::reembed_copy_get(&st.http, &p, source, job).await?;
    Ok(Some(match v.state.as_str() {
        "done" => Polled::Done {
            chunks: Some(v.chunks_total),
            missing: json!([]),
        },
        "failed" => Polled::Failed(
            v.error
                .clone()
                .unwrap_or_else(|| "the re-embed copy failed".to_string()),
        ),
        _ => Polled::Running {
            chunks: Some(v.chunks_done),
        },
    }))
}

fn chunks_i32(n: Option<i64>) -> Option<i32> {
    n.map(|n| i32::try_from(n).unwrap_or(i32::MAX))
}

/// Bring the tenant's `building` generations up to date with strata, then the changes waiting on them ([`catch_up`]
/// first). A strata that cannot be read leaves them as they are (a read never fails because strata is down).
pub async fn refresh(st: &AppState, tenant: Uuid) -> ApiResult<()> {
    let (gens, all) = {
        let mut tx = begin_scoped(&st.db, tenant).await?;
        catch_up(&mut tx, tenant, SWEEPER).await?;
        let g = generations::building(&mut tx).await?;
        let a = stores::all(&mut tx).await?;
        tx.commit().await?;
        (g, a)
    };
    if gens.is_empty() {
        return Ok(());
    }
    let mut listing: Option<Vec<rag::StoreView>> = None;
    let mut polled: Vec<(Uuid, Polled, Map<String, Value>)> = Vec::new();
    for g in &gens {
        let p = match poll(st, tenant, g).await {
            Ok(Some(p)) => p,
            Ok(None) => continue,
            Err(e) => {
                tracing::warn!(generation = %g.id, code = e.code(), "rebuild status not readable");
                continue;
            }
        };
        let mut members = Map::new();
        if let (Polled::Done { .. }, Some(sid), Some(root)) = (
            &p,
            g.strata_store_id,
            all.iter().find(|s| s.id == g.store_id),
        ) {
            if listing.is_none() {
                let read = match strata::plane(st, tenant, Service::Rag).await {
                    Ok(rp) => rag::stores(&st.http, &rp).await,
                    Err(e) => Err(e),
                };
                match read {
                    Ok(l) => listing = Some(l),
                    Err(e) => {
                        tracing::warn!(code = e.code(), "store listing not readable");
                        continue;
                    }
                }
            }
            if let Some(l) = &listing {
                members = generations::mirror_members(&all, root, StrataStoreId(sid), l);
            }
        }
        polled.push((g.id, p, members));
    }
    let mut tx = begin_scoped(&st.db, tenant).await?;
    for (gid, p, members) in polled {
        match p {
            Polled::Running { chunks } => {
                sqlx::query(
                    "UPDATE store_generation SET chunks = $2, updated_at = clock_timestamp() \
                      WHERE id = $1 AND state = 'building'",
                )
                .bind(gid)
                .bind(chunks_i32(chunks))
                .execute(&mut *tx)
                .await?;
            }
            Polled::Done { chunks, missing } => {
                sqlx::query(
                    "UPDATE store_generation \
                        SET state = 'ready', chunks = $2, members = $3, missing = $4, \
                            updated_at = clock_timestamp() \
                      WHERE id = $1 AND state = 'building'",
                )
                .bind(gid)
                .bind(chunks_i32(chunks))
                .bind(Value::Object(members))
                .bind(missing)
                .execute(&mut *tx)
                .await?;
            }
            Polled::Failed(error) => {
                sqlx::query(
                    "UPDATE store_generation SET state = 'failed', error = $2, updated_at = clock_timestamp() \
                      WHERE id = $1 AND state = 'building'",
                )
                .bind(gid)
                .bind(error.chars().take(500).collect::<String>())
                .execute(&mut *tx)
                .await?;
            }
        }
    }
    settle(&mut tx).await?;
    tx.commit().await?;
    Ok(())
}

/// Every `reembedding` change job: all targets ready → `ready` (chunks summed); a failed or vanished target →
/// `pending` again without it (the person starts the rebuild again).
async fn settle(c: &mut PgConnection) -> ApiResult<()> {
    let rows: Vec<(Uuid, Uuid, Value)> = sqlx::query_as(
        "SELECT model_change_id, job_id, stores FROM model_change_job WHERE state = 'reembedding' \
          ORDER BY job_id, model_change_id FOR UPDATE",
    )
    .fetch_all(&mut *c)
    .await?;
    for (id, job, v) in rows {
        let mut entries = entries_of(&v)?;
        let ids: Vec<Uuid> = entries.iter().filter_map(|e| e.to_generation).collect();
        let gens = generations::by_ids(&mut *c, &ids).await?;
        let mut lost = false;
        for e in entries.iter_mut() {
            let ok = e
                .to_generation
                .and_then(|g| gens.get(&g))
                .is_some_and(|g| g.state == generations::BUILDING || g.is_ready());
            if !ok {
                e.to_generation = None;
                lost = true;
            }
        }
        let all_ready = entries.iter().all(|e| {
            e.to_generation
                .and_then(|g| gens.get(&g))
                .is_some_and(Generation::is_ready)
        });
        let chunks: i64 = entries
            .iter()
            .filter_map(|e| e.to_generation.and_then(|g| gens.get(&g)))
            .filter_map(|g| g.chunks)
            .map(i64::from)
            .sum();
        let state = if lost {
            "pending"
        } else if all_ready {
            "ready"
        } else {
            continue;
        };
        sqlx::query(
            "UPDATE model_change_job SET state = $3, stores = $4, chunks = $5, updated_at = clock_timestamp() \
              WHERE model_change_id = $1 AND job_id = $2",
        )
        .bind(id)
        .bind(job)
        .bind(state)
        .bind(entries_json(&entries)?)
        .bind(chunks_i32(Some(chunks)))
        .execute(&mut *c)
        .await?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------------------------
// Accept (acceptModelChange) and publish

/// `acceptModelChange`: every target generation ready → the job's `embedding_pin` drafts (module doc).
pub async fn accept(
    st: &AppState,
    reg: &Registry,
    j: &JobCtx,
    id: Uuid,
    acknowledge_missing: bool,
) -> ApiResult<Value> {
    let tenant = j.t.tenant;
    refresh(st, tenant).await?;
    let mut tx = j.begin(&st.db).await?;
    tenancy::lock_tenant_row(&mut tx, tenant).await?;
    // lock order: tenants → the job (as `design::track` and a publish take it) → its model-change rows
    sqlx::query("SELECT 1 FROM jobs WHERE id = $1 FOR UPDATE")
        .bind(j.job_id)
        .execute(&mut *tx)
        .await?;
    let ch = change(&mut tx, id).await?.ok_or_else(not_found)?;
    let cj = change_job(&mut tx, id, j.job_id, true)
        .await?
        .ok_or_else(not_found)?;
    if matches!(cj.state.as_str(), "accepted" | "published") {
        return Err(already_accepted());
    }
    if cj.state == "dismissed" {
        return Err(superseded());
    }
    let entries = entries_of(&cj.stores)?;
    let ids: Vec<Uuid> = entries.iter().filter_map(|e| e.to_generation).collect();
    let gens = generations::by_ids(&mut tx, &ids).await?;
    let all = stores::all(&mut tx).await?;
    let key_of = |sid: Uuid| {
        all.iter()
            .find(|s| s.id == sid)
            .map_or_else(|| sid.to_string(), |s| s.key.clone())
    };
    let not_ready: Vec<String> = entries
        .iter()
        .filter(|e| {
            e.to_generation
                .and_then(|g| gens.get(&g))
                .is_none_or(|g| !g.is_ready())
        })
        .map(|e| key_of(e.store_id))
        .collect();
    if !not_ready.is_empty() {
        return Err(ApiError::detailed(
            axum::http::StatusCode::CONFLICT,
            "generation_not_ready",
            "Rebuild these stores first; they are not ready yet.",
        )
        .with("stores", not_ready));
    }
    let missing: Vec<Value> = entries
        .iter()
        .filter_map(|e| e.to_generation.and_then(|g| gens.get(&g)))
        .flat_map(Generation::missing_originals)
        .collect();
    if !missing.is_empty() && !acknowledge_missing {
        return Err(ApiError::detailed(
            axum::http::StatusCode::CONFLICT,
            "originals_not_retained",
            "strata_ingest kept no original for some documents: re-upload or re-sync them after accepting.",
        )
        .with("documents", missing));
    }
    let from = ch
        .from_model
        .clone()
        .unwrap_or_else(|| "default".to_string());
    let to = ch.to_model.clone().unwrap_or_else(|| "default".to_string());
    for e in &entries {
        let (Some(gid), Some(root)) = (e.to_generation, all.iter().find(|s| s.id == e.store_id))
        else {
            return Err(stores::store_not_found());
        };
        let Some(g) = gens.get(&gid) else {
            return Err(not_found());
        };
        let models = g.models().to_value();
        let w = DraftWrite {
            area: AREA,
            page: PAGE,
            kind: generations::KIND,
            r#ref: targets::store_ref(&root.key),
            label: Label::Now(format!(
                "Models of {}: {} {from} \u{2192} {to}",
                root.key, ch.slot
            )),
        };
        let (job_id, key, by) = (j.job_id, root.key.clone(), j.t.caller.sub.clone());
        design::track::<(), _>(&mut tx, reg, j, w, move |c| {
            Box::pin(
                async move { targets::write_pin(c, tenant, job_id, &key, gid, &models, &by).await },
            )
        })
        .await?;
    }
    set_job_state(&mut tx, id, j.job_id, "accepted", &entries, &j.t.caller.sub).await?;
    // review I4: a withheld embed push is tried again (the sender withholds it again while a job still reads a
    // default-following root)
    crate::providers::slots::requeue_withheld(&mut tx, tenant).await?;
    audit::record(
        &mut tx,
        &j.t.actor(),
        Event::new("model_change.accept", "model_change", id.to_string())
            .job(&j.job_key)
            .detail("slot", ch.slot.clone())
            .detail("stores", entries.len())
            .detail("draft", true),
    )
    .await?;
    let cj = change_job(&mut tx, id, j.job_id, false)
        .await?
        .ok_or_else(not_found)?;
    let v = job_view(&mut tx, &ch, &cj).await?;
    tx.commit().await?;
    Ok(v)
}

/// The `embedding_pin` `published` hook: `job` published the pin of root store `store_key`. Every accepted change of
/// the job whose stores are all live on their target generation is `published`; the store's generations nothing uses
/// any more are retired.
pub async fn on_published(
    c: &mut PgConnection,
    tenant: Uuid,
    job: &JobRef,
    store_key: &str,
) -> ApiResult<()> {
    let all = stores::all(&mut *c).await?;
    let Some(root) = all
        .iter()
        .find(|s| s.key == store_key && s.parent_id.is_none())
    else {
        return Ok(());
    };
    let rows: Vec<(Uuid, Value)> = sqlx::query_as(
        "SELECT model_change_id, stores FROM model_change_job \
          WHERE job_id = $1 AND state = 'accepted' ORDER BY model_change_id FOR UPDATE",
    )
    .bind(job.id)
    .fetch_all(&mut *c)
    .await?;
    for (id, v) in rows {
        let entries = entries_of(&v)?;
        let mut live_all = true;
        for e in &entries {
            let Some(r) = all.iter().find(|s| s.id == e.store_id) else {
                live_all = false;
                break;
            };
            let live = generations::pinned(&mut *c, job, r, &DesignOf::Live)
                .await?
                .map(|g| g.id);
            if e.to_generation.is_none() || live != e.to_generation {
                live_all = false;
                break;
            }
        }
        if live_all {
            sqlx::query(
                "UPDATE model_change_job SET state = 'published', updated_by = $3, updated_at = clock_timestamp() \
                  WHERE model_change_id = $1 AND job_id = $2",
            )
            .bind(id)
            .bind(job.id)
            .bind(PUBLISHER)
            .execute(&mut *c)
            .await?;
        }
    }
    generations::retire_unpinned(c, tenant, root.id, &Actor::system(PUBLISHER)).await?;
    // ruling 2026-10-21 (revised): an embed change withheld while jobs read roots on the deployment default goes
    // out again now that this job reads its new generation (the sender withholds it again if another job still does)
    crate::providers::slots::requeue_withheld(c, tenant).await?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------------------------
// Sweeper (review I3, I4, I6)

/// What one [`sweep`] did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SweepReport {
    pub tenants: usize,
    /// `building` generations failed because their rebuild never started.
    pub stranded: usize,
    /// strata roots such a rebuild had made, queued for deletion.
    pub orphans: usize,
    /// Job parts [`catch_up`] wrote.
    pub caught_up: usize,
    /// Withheld embed pushes queued again.
    pub requeued: usize,
    /// Tenants whose sweep failed (logged; tried again next sweep).
    pub errors: usize,
}

impl SweepReport {
    /// The sweep did or hit something.
    pub fn changed(&self) -> bool {
        self.stranded + self.orphans + self.caught_up + self.requeued + self.errors > 0
    }
}

/// The model-change sweeper, piggybacked on the outbox drainer (boot, then every
/// crate::provision::outbox::SWEEP_EVERY ticks; no CronJob): [`sweep_tenant`] for every active tenant. It never starts
/// a rebuild — a rebuild is a person's click (D82: no timer triggers a GPU call); a stranded one goes back to that
/// person as a `pending` change.
pub async fn sweep(st: &AppState) -> ApiResult<SweepReport> {
    let ids: Vec<Uuid> = {
        let mut tx = crate::platform_db::begin_platform(&st.db).await?;
        let ids: Vec<Uuid> =
            sqlx::query_scalar("SELECT id FROM agent_list_tenants() WHERE status = 'active'")
                .fetch_all(&mut *tx)
                .await?;
        tx.commit().await?;
        ids
    };
    let mut r = SweepReport::default();
    for tenant in ids {
        r.tenants += 1;
        if let Err(e) = sweep_tenant(st, tenant, &mut r).await {
            r.errors += 1;
            tracing::warn!(%tenant, code = e.code(), error = %e, "model_change_sweep_tenant_failed");
        }
    }
    Ok(r)
}

/// One tenant's sweep.
///
/// - review I6: a `building` generation that never recorded its rebuild job ([`generations::stranded`], the process
///   died between claiming it and starting the rebuild) is `failed`; a strata root that rebuild already made (found by
///   its name, `<key>.gen-<short>`) is queued for deletion (`store_delete`); the change parts waiting on it go back to
///   `pending` (`settle`), so the person starts the rebuild again. strata unreadable = tried again next sweep.
/// - review I3: [`catch_up`] (jobs and roots that appeared after a change).
/// - review I4: a withheld embed push is queued again (a blocking root or job may be gone).
pub async fn sweep_tenant(st: &AppState, tenant: Uuid, r: &mut SweepReport) -> ApiResult<()> {
    let (stuck, all) = {
        let mut tx = begin_scoped(&st.db, tenant).await?;
        let g = generations::stranded(&mut tx).await?;
        let a = stores::all(&mut tx).await?;
        tx.commit().await?;
        (g, a)
    };
    let mut fail: Vec<Uuid> = Vec::new();
    let mut orphans: Vec<StrataStoreId> = Vec::new();
    if !stuck.is_empty() {
        match strata::plane(st, tenant, Service::Rag).await {
            // no data plane for the tenant: the rebuild cannot have created anything in strata
            Err(_) => fail.extend(stuck.iter().map(|g| g.id)),
            Ok(p) => match rag::stores(&st.http, &p).await {
                Ok(listing) => {
                    for g in &stuck {
                        if let Some(root) = all.iter().find(|s| s.id == g.store_id) {
                            let name = gen_root_name(&root.key, g.id);
                            orphans.extend(
                                listing
                                    .iter()
                                    .filter(|v| v.parent_id.is_none() && v.name == name)
                                    .map(|v| v.id),
                            );
                        }
                        fail.push(g.id);
                    }
                }
                Err(e) => {
                    tracing::warn!(%tenant, code = e.code(), "stranded rebuilds kept: store listing not readable");
                }
            },
        }
    }
    let mut tx = begin_scoped(&st.db, tenant).await?;
    for gid in &fail {
        let done = sqlx::query(
            "UPDATE store_generation \
                SET state = 'failed', error = 'The rebuild never started in strata (the service restarted); start it again.', \
                    updated_at = clock_timestamp() \
              WHERE id = $1 AND state = 'building' AND reembed_job IS NULL",
        )
        .bind(gid)
        .execute(&mut *tx)
        .await?;
        if done.rows_affected() > 0 {
            r.stranded += 1;
        }
    }
    let actor = Actor::system(SWEEPER);
    for sid in &orphans {
        store_delete::enqueue(&mut tx, tenant, *sid, &actor).await?;
        r.orphans += 1;
    }
    if !fail.is_empty() {
        settle(&mut tx).await?;
    }
    r.caught_up += catch_up(&mut tx, tenant, SWEEPER).await?;
    if crate::providers::slots::requeue_withheld(&mut tx, tenant)
        .await?
        .is_some()
    {
        r.requeued += 1;
    }
    tx.commit().await?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------------------------
// Views (modelChanges, modelChange)

/// One job's part of a change: `{job, state, stores[{store, store_id, from_generation, to_generation, chunks,
/// missing_originals}], chunks, live_release, draft_rev}`.
async fn job_view(c: &mut PgConnection, ch: &Change, cj: &ChangeJob) -> ApiResult<Value> {
    let entries = entries_of(&cj.stores)?;
    let mut ids: Vec<Uuid> = entries.iter().map(|e| e.from_generation).collect();
    ids.extend(entries.iter().filter_map(|e| e.to_generation));
    let gens = generations::by_ids(&mut *c, &ids).await?;
    let all = stores::all(&mut *c).await?;
    Ok(job_json(ch, cj, &entries, &gens, &all))
}

fn job_json(
    ch: &Change,
    cj: &ChangeJob,
    entries: &[StoreEntry],
    gens: &HashMap<Uuid, Generation>,
    all: &[StoreRow],
) -> Value {
    let stores: Vec<Value> = entries
        .iter()
        .map(|e| {
            let to = e.to_generation.and_then(|g| gens.get(&g));
            json!({
                "store": all.iter().find(|s| s.id == e.store_id).map(|s| s.key.clone()),
                "store_id": e.store_id,
                "from_generation": gens.get(&e.from_generation).map(Generation::json),
                "to_generation": to.map(Generation::json),
                "chunks": to.and_then(|g| g.chunks),
                "missing_originals": to.map_or_else(Vec::new, Generation::missing_originals),
            })
        })
        .collect();
    json!({
        "model_change_id": ch.id,
        "job": cj.job_key,
        "state": cj.state,
        "stores": stores,
        "chunks": cj.chunks,
        "live_release": cj.live_release,
        "draft_rev": cj.draft_rev,
        "updated_at": cj.updated_at,
    })
}

/// `modelChanges`: the tenant's changes, newest first, each with its jobs' states; `state` keeps the changes with a
/// job in that state.
pub async fn list(c: &mut PgConnection, state: Option<&str>) -> ApiResult<Vec<Value>> {
    let changes: Vec<Change> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {CHANGE_COLS} FROM model_change ORDER BY created_at DESC, id"
    )))
    .fetch_all(&mut *c)
    .await?;
    let mut out = Vec::new();
    for ch in changes {
        let jobs = change_jobs(&mut *c, ch.id).await?;
        if state.is_some_and(|s| !jobs.iter().any(|j| j.state == s)) {
            continue;
        }
        let mut v = ch.json();
        v["jobs"] = json!(
            jobs.iter()
                .map(|j| json!({"job": j.job_key, "state": j.state}))
                .collect::<Vec<_>>()
        );
        out.push(v);
    }
    Ok(out)
}

/// `modelChange`: one change with every affected job's part; 404 `model_change_not_found`.
pub async fn view(c: &mut PgConnection, id: Uuid) -> ApiResult<Value> {
    let ch = change(&mut *c, id).await?.ok_or_else(not_found)?;
    let jobs = change_jobs(&mut *c, id).await?;
    let mut out = Vec::with_capacity(jobs.len());
    for cj in &jobs {
        out.push(job_view(&mut *c, &ch, cj).await?);
    }
    let mut v = ch.json();
    v["jobs"] = Value::Array(out);
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_ref_marks_older_models_inherited() {
        let working = ModelSet::default()
            .with(Slot::Embed, Some("e2"))
            .with(Slot::Rerank, Some("r2"));
        let refs = json!({"embed": {"source": "tenant", "slot_version": 3},
                          "rerank": {"source": "tenant", "slot_version": 1},
                          "chat": {"source": "default"}});
        let target = working.with(Slot::Rerank, Some("r1"));
        let r = provider_ref(&target, &working, &refs);
        assert_eq!(r["embed"], json!({"source": "tenant", "slot_version": 3}));
        assert_eq!(r["rerank"], json!({"source": "inherited"}));
        assert_eq!(r["chat"], json!({"source": "default"}));
    }

    #[test]
    fn store_entries_round_trip() {
        let e = vec![StoreEntry {
            store_id: Uuid::from_u128(1),
            from_generation: Uuid::from_u128(2),
            to_generation: None,
        }];
        let v = entries_json(&e).unwrap();
        assert_eq!(entries_of(&v).unwrap(), e);
    }
}
