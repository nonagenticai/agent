//! Store generations (B3 T4a; D83, D92, D93; plan §4 0056, §0 R7): one strata ROOT per model set of an agent root
//! store, and which one a job uses.
//!
//! - A generation is a model set (`embed`, `rerank`, `chat`, `ocr`, `vision`, `transcribe`; the slot's model name,
//!   `None` = the deployment default) and the strata tree built with it. Generation `initial` (the models strata's
//!   tenant row runs when the store is created, [`effective_models`]) is the store's own
//!   binding (`knowledge_store.strata_store_id`, created by `store_ensure`); every rebuild is a NEW strata root whose
//!   subtree strata mirrors from its source generation (`members` maps each agent store of the tree to its strata
//!   store there). One embed model per tree (R7): a model is never changed in place.
//! - A job pins one generation per root store: the ledger kind `embedding_pin` (crate::knowledge::targets, key
//!   `embedding:<store_key>`). No pin = the store's `initial` generation. Every search and ingest of a job resolves
//!   `store → pinned generation → strata store` per [`DesignOf`] ([`root_for`], [`strata_for`]): Production reads the
//!   live pin, the Test environment the draft pin, so a job never searches vectors of a model it has not published.
//! - A generation no job pins (draft or live), not the newest ready one of its store, not `initial` and not the target
//!   of an open model change is retired when a job publishes ([`retire_unpinned`]): outbox op `store_delete` removes
//!   its strata root (event-driven, never scheduled).
use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use super::stores::{self, StoreRow};
use crate::audit::Actor;
use crate::design::{DesignOf, JobRef, snap_from_json};
use crate::error::{ApiError, ApiResult};
use crate::providers::kinds::Slot;
use crate::provision::senders::store_delete;
use crate::strata::StrataStoreId;

/// The ledger kind that pins a job's generation of a root store.
pub const KIND: &str = "embedding_pin";
/// How a generation's content was made.
pub const INITIAL: &str = "initial";
/// Generation states.
pub const BUILDING: &str = "building";
pub const READY: &str = "ready";
pub const FAILED: &str = "failed";
pub const RETIRED: &str = "retired";

/// The rebuild a change of `slot` needs (D93): `embed` re-embeds (`reembed`), `rerank` / `chat` copy the vectors with
/// new store bindings (`copy`, no GPU embed call), `ocr` / `vision` / `transcribe` re-run extraction on the retained
/// originals (`reingest`).
pub fn rebuild_of(slot: Slot) -> &'static str {
    match slot {
        Slot::Embed => "reembed",
        Slot::Rerank | Slot::Chat => "copy",
        Slot::Ocr | Slot::Vision | Slot::Transcribe => "reingest",
    }
}

/// The six models of a generation (`None` = the deployment default).
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ModelSet {
    #[serde(default)]
    pub embed: Option<String>,
    #[serde(default)]
    pub rerank: Option<String>,
    #[serde(default)]
    pub chat: Option<String>,
    #[serde(default)]
    pub ocr: Option<String>,
    #[serde(default)]
    pub vision: Option<String>,
    #[serde(default)]
    pub transcribe: Option<String>,
}

impl ModelSet {
    pub fn get(&self, slot: Slot) -> Option<&str> {
        match slot {
            Slot::Embed => self.embed.as_deref(),
            Slot::Rerank => self.rerank.as_deref(),
            Slot::Chat => self.chat.as_deref(),
            Slot::Ocr => self.ocr.as_deref(),
            Slot::Vision => self.vision.as_deref(),
            Slot::Transcribe => self.transcribe.as_deref(),
        }
    }

    /// This set with `slot` running `model`.
    pub fn with(&self, slot: Slot, model: Option<&str>) -> Self {
        let mut m = self.clone();
        let v = model.map(str::to_string);
        match slot {
            Slot::Embed => m.embed = v,
            Slot::Rerank => m.rerank = v,
            Slot::Chat => m.chat = v,
            Slot::Ocr => m.ocr = v,
            Slot::Vision => m.vision = v,
            Slot::Transcribe => m.transcribe = v,
        }
        m
    }

    /// `{embed, rerank, chat, ocr, vision, transcribe}` (null = the deployment default).
    pub fn to_value(&self) -> Value {
        json!({
            "embed": self.embed,
            "rerank": self.rerank,
            "chat": self.chat,
            "ocr": self.ocr,
            "vision": self.vision,
            "transcribe": self.transcribe,
        })
    }
}

/// One `store_generation` row.
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Generation {
    pub id: Uuid,
    pub store_id: Uuid,
    pub strata_store_id: Option<Uuid>,
    pub embed: Option<String>,
    pub rerank: Option<String>,
    pub chat: Option<String>,
    pub ocr: Option<String>,
    pub vision: Option<String>,
    pub transcribe: Option<String>,
    pub build: String,
    pub state: String,
    pub source_id: Option<Uuid>,
    pub reembed_job: Option<String>,
    pub members: Value,
    pub chunks: Option<i32>,
    pub missing: Value,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
}

const COLS: &str = "id, store_id, strata_store_id, embed, rerank, chat, ocr, vision, transcribe, build, state, \
                    source_id, reembed_job, members, chunks, missing, error, created_at";

impl Generation {
    pub fn models(&self) -> ModelSet {
        ModelSet {
            embed: self.embed.clone(),
            rerank: self.rerank.clone(),
            chat: self.chat.clone(),
            ocr: self.ocr.clone(),
            vision: self.vision.clone(),
            transcribe: self.transcribe.clone(),
        }
    }

    pub fn is_initial(&self) -> bool {
        self.build == INITIAL
    }

    pub fn is_ready(&self) -> bool {
        self.state == READY
    }

    /// The strata root of this generation: its own, or (initial) the root store's binding.
    pub fn root_sid(&self, root: &StoreRow) -> Option<StrataStoreId> {
        if self.is_initial() {
            root.strata()
        } else {
            self.strata_store_id.map(StrataStoreId)
        }
    }

    /// The strata store of `member` (an agent store of `root`'s tree) in this generation.
    pub fn member_sid(&self, member: &StoreRow, root: &StoreRow) -> Option<StrataStoreId> {
        if self.is_initial() {
            return member.strata();
        }
        if member.id == root.id {
            return self.strata_store_id.map(StrataStoreId);
        }
        self.members
            .get(member.id.to_string().as_str())
            .and_then(Value::as_str)
            .and_then(|s| Uuid::parse_str(s).ok())
            .map(StrataStoreId)
    }

    /// The documents a `reingest` rebuild could not copy (strata_ingest `missing_originals`).
    pub fn missing_originals(&self) -> Vec<Value> {
        self.missing.as_array().cloned().unwrap_or_default()
    }

    /// The generation as the model-change views show it.
    pub fn json(&self) -> Value {
        json!({
            "id": self.id,
            "store_id": self.store_id,
            "strata_store_id": self.strata_store_id,
            "models": self.models().to_value(),
            "build": self.build,
            "state": self.state,
            "chunks": self.chunks,
            "missing_originals": self.missing,
            "error": self.error,
            "created_at": self.created_at,
        })
    }
}

/// One generation by id.
pub async fn by_id(c: &mut PgConnection, id: Uuid) -> ApiResult<Option<Generation>> {
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {COLS} FROM store_generation WHERE id = $1"
    )))
    .bind(id)
    .fetch_optional(&mut *c)
    .await?)
}

/// Every generation of root store `store`, oldest first.
pub async fn of_store(c: &mut PgConnection, store: Uuid) -> ApiResult<Vec<Generation>> {
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {COLS} FROM store_generation WHERE store_id = $1 ORDER BY created_at, id"
    )))
    .bind(store)
    .fetch_all(&mut *c)
    .await?)
}

/// The `building` / `ready` generation of `store` running exactly `models`.
pub async fn with_models(
    c: &mut PgConnection,
    store: Uuid,
    models: &ModelSet,
) -> ApiResult<Option<Generation>> {
    Ok(of_store(c, store)
        .await?
        .into_iter()
        .find(|g| (g.state == BUILDING || g.state == READY) && g.models() == *models))
}

/// Every `building` generation with a rebuild job in strata.
pub async fn building(c: &mut PgConnection) -> ApiResult<Vec<Generation>> {
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {COLS} FROM store_generation \
          WHERE state = 'building' AND reembed_job IS NOT NULL ORDER BY created_at, id"
    )))
    .fetch_all(&mut *c)
    .await?)
}

/// How long a `building` generation may wait for its rebuild job before the sweeper fails it (review I6): a rebuild
/// records its strata job within seconds of claiming the generation (crate::knowledge::model_changes::reembed).
pub const STRANDED_AFTER_MINUTES: i32 = 15;

/// Every `building` generation that never recorded a rebuild job (the process died between claiming it and starting
/// the rebuild in strata) and is older than [`STRANDED_AFTER_MINUTES`].
pub async fn stranded(c: &mut PgConnection) -> ApiResult<Vec<Generation>> {
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {COLS} FROM store_generation \
          WHERE state = 'building' AND reembed_job IS NULL \
            AND created_at < clock_timestamp() - make_interval(mins => $1) ORDER BY created_at, id"
    )))
    .bind(STRANDED_AFTER_MINUTES)
    .fetch_all(&mut *c)
    .await?)
}

/// One working `provider_slot` row, as a generation reads it.
#[derive(sqlx::FromRow)]
struct SlotModel {
    slot: String,
    configured: bool,
    model: Option<String>,
    version: i32,
}

/// The tenant's current model set (its working slot rows; a slot not configured runs the deployment default) and
/// the matching `provider_ref` (`{<slot>: {source, slot_version}}`).
pub async fn tenant_models(c: &mut PgConnection, tenant: Uuid) -> ApiResult<(ModelSet, Value)> {
    let rows: Vec<SlotModel> = sqlx::query_as(
        "SELECT slot, configured, model, version FROM provider_slot WHERE tenant_id = $1",
    )
    .bind(tenant)
    .fetch_all(&mut *c)
    .await?;
    let mut set = ModelSet::default();
    let mut refs = Map::new();
    for slot in Slot::ALL {
        let row = rows
            .iter()
            // a slot reset to the deployment default (0057) is configured without a model
            .find(|r| r.slot == slot.as_str() && r.configured && r.model.is_some());
        let entry = match row {
            Some(r) => {
                set = set.with(slot, r.model.as_deref());
                json!({"source": "tenant", "slot_version": r.version})
            }
            None => json!({"source": "default"}),
        };
        refs.insert(slot.as_str().to_string(), entry);
    }
    Ok((set, Value::Object(refs)))
}

/// One `provider_slot_remote` row, as [`effective_models`] reads it.
#[derive(sqlx::FromRow)]
struct RemoteModel {
    slot: String,
    pushed_version: i32,
    observed: Option<Value>,
}

/// What strata's tenant row actually runs (review I3), from what agent last observed or pushed
/// (`provider_slot_remote.observed`: every push records strata's answer, every slots read refreshes it): a slot whose
/// observed source is `tenant` runs that registration's model; anything else (`default`, `unconfigured`, never
/// observed, an embed push withheld) runs the deployment default (`None`). Never the draft: a saved but unpublished
/// model is not what strata embeds with. With the matching `provider_ref` (`{source, slot_version}`).
pub async fn effective_models(c: &mut PgConnection, tenant: Uuid) -> ApiResult<(ModelSet, Value)> {
    let rows: Vec<RemoteModel> = sqlx::query_as(
        "SELECT slot, pushed_version, observed FROM provider_slot_remote WHERE tenant_id = $1",
    )
    .bind(tenant)
    .fetch_all(&mut *c)
    .await?;
    let mut set = ModelSet::default();
    let mut refs = Map::new();
    for slot in Slot::ALL {
        let reg = rows
            .iter()
            .filter(|r| r.slot == slot.as_str())
            .find_map(|r| {
                let o = r.observed.as_ref()?;
                if o.get("source").and_then(Value::as_str) != Some("tenant") {
                    return None;
                }
                let model = o.get("model").and_then(Value::as_str)?;
                Some((model.to_string(), r.pushed_version))
            });
        let entry = match reg {
            Some((model, version)) => {
                set = set.with(slot, Some(model.as_str()));
                json!({"source": "tenant", "slot_version": version})
            }
            None => json!({"source": "default"}),
        };
        refs.insert(slot.as_str().to_string(), entry);
    }
    Ok((set, Value::Object(refs)))
}

/// Give every root store of the scoped tenant its `initial` generation when it has none: the models strata's tenant
/// row runs now ([`effective_models`], review I3 — a store created while a new model is only drafted, or while an
/// embed push is withheld, is embedded with what strata runs). Called when a root store is created and before a slot
/// change fans out.
pub async fn ensure_initial_all(c: &mut PgConnection, tenant: Uuid, by: &str) -> ApiResult<()> {
    let (m, refs) = effective_models(&mut *c, tenant).await?;
    sqlx::query(
        "INSERT INTO store_generation (id, store_id, embed, rerank, chat, ocr, vision, transcribe, provider_ref, \
                                       build, state, created_by) \
         SELECT gen_random_uuid(), s.id, $1, $2, $3, $4, $5, $6, $7, 'initial', 'ready', $8 \
           FROM knowledge_store s WHERE s.parent_id IS NULL \
         ON CONFLICT (tenant_id, store_id) WHERE build = 'initial' DO NOTHING",
    )
    .bind(&m.embed)
    .bind(&m.rerank)
    .bind(&m.chat)
    .bind(&m.ocr)
    .bind(&m.vision)
    .bind(&m.transcribe)
    .bind(refs)
    .bind(by)
    .execute(&mut *c)
    .await?;
    Ok(())
}

/// The `initial` generation of `store`.
pub async fn initial(c: &mut PgConnection, store: Uuid) -> ApiResult<Option<Generation>> {
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {COLS} FROM store_generation WHERE store_id = $1 AND build = 'initial'"
    )))
    .bind(store)
    .fetch_optional(&mut *c)
    .await?)
}

/// The job `key` of the scoped tenant; 404 `job_not_found`.
pub async fn job_ref(c: &mut PgConnection, key: &str) -> ApiResult<JobRef> {
    let id: Option<Uuid> = sqlx::query_scalar("SELECT id FROM jobs WHERE key = $1")
        .bind(key)
        .fetch_optional(&mut *c)
        .await?;
    let id = id.ok_or_else(|| ApiError::Missing("job_not_found", format!("no job {key:?}")))?;
    Ok(JobRef {
        id,
        key: key.to_string(),
    })
}

/// The generation id a pin value names.
pub fn pinned_id(v: &Value) -> Option<Uuid> {
    v.get("generation")
        .and_then(Value::as_str)
        .and_then(|s| Uuid::parse_str(s).ok())
}

/// The generation id `job` pins for root store `store_key` in its design `of`: the working row (`Draft`), the current
/// release's snapshot (`Live`; a job never released reads its baseline) or a stored release. `None` = no pin. Reads
/// the B2 tables directly (the shape of `design::read::value` for this kind) so the kind's own `published` hook,
/// which has no registry, reads exactly what every other caller reads; Production never falls back to the draft.
pub async fn pin_of(
    c: &mut PgConnection,
    job: &JobRef,
    store_key: &str,
    of: &DesignOf,
) -> ApiResult<Option<Uuid>> {
    let key = format!("embedding:{store_key}");
    let stored: Option<Option<Value>> = match of {
        DesignOf::Draft => {
            let id: Option<Uuid> = sqlx::query_scalar(
                "SELECT generation_id FROM job_embedding_pin WHERE job_id = $1 AND store_key = $2",
            )
            .bind(job.id)
            .bind(store_key)
            .fetch_optional(&mut *c)
            .await?;
            return Ok(id);
        }
        DesignOf::Live => {
            let current: Option<Option<String>> =
                sqlx::query_scalar("SELECT current_release_id FROM jobs WHERE id = $1")
                    .bind(job.id)
                    .fetch_optional(&mut *c)
                    .await?;
            match current.flatten() {
                Some(release) => release_snap(c, job.id, &release, &key).await?,
                None => {
                    sqlx::query_scalar(
                        "SELECT snap FROM release_baseline WHERE job_id = $1 AND key = $2",
                    )
                    .bind(job.id)
                    .bind(&key)
                    .fetch_optional(&mut *c)
                    .await?
                }
            }
        }
        DesignOf::Release(release) => release_snap(c, job.id, release, &key).await?,
    };
    let snap = snap_from_json(stored.flatten())?;
    Ok(snap.as_ref().and_then(pinned_id))
}

async fn release_snap(
    c: &mut PgConnection,
    job: Uuid,
    release: &str,
    key: &str,
) -> ApiResult<Option<Option<Value>>> {
    Ok(sqlx::query_scalar(
        "SELECT snap FROM release_snapshot WHERE job_id = $1 AND release_id = $2 AND key = $3",
    )
    .bind(job)
    .bind(release)
    .bind(key)
    .fetch_optional(&mut *c)
    .await?)
}

/// The generation `job` uses for root store `root` in its design `of`: the pinned one, else (no pin, or a pin naming
/// a generation of another store) the store's `initial` generation. `None` = the store has no generation yet (its own
/// binding is what the job uses).
pub async fn pinned(
    c: &mut PgConnection,
    job: &JobRef,
    root: &StoreRow,
    of: &DesignOf,
) -> ApiResult<Option<Generation>> {
    if let Some(id) = pin_of(&mut *c, job, &root.key, of).await?
        && let Some(g) = by_id(&mut *c, id).await?
        && g.store_id == root.id
    {
        return Ok(Some(g));
    }
    initial(c, root.id).await
}

/// The strata root `job` searches for root store `root` in its design `of` (§0 R7: every query names it).
pub async fn root_for(
    c: &mut PgConnection,
    job: &JobRef,
    root: &StoreRow,
    of: &DesignOf,
) -> ApiResult<Option<StrataStoreId>> {
    Ok(match pinned(c, job, root, of).await? {
        Some(g) => g.root_sid(root),
        None => root.strata(),
    })
}

/// The strata store of agent store `store` (any store of a tree) under `job`'s pinned generation of its root, in its
/// design `of`: what searches and ingests of the job target (T6, T8, T10 `leaf_set`). `None` = not bound there.
pub async fn strata_for(
    c: &mut PgConnection,
    job: &JobRef,
    store: Uuid,
    of: &DesignOf,
) -> ApiResult<Option<StrataStoreId>> {
    let all = stores::all(&mut *c).await?;
    let Some(root_id) = stores::root_of(&all, store) else {
        return Ok(None);
    };
    let (Some(root), Some(member)) = (
        all.iter().find(|s| s.id == root_id),
        all.iter().find(|s| s.id == store),
    ) else {
        return Ok(None);
    };
    Ok(match pinned(c, job, root, of).await? {
        Some(g) => g.member_sid(member, root),
        None => member.strata(),
    })
}

/// The strata stores of `root`'s tree in a rebuilt generation whose strata root is `new_root`: each agent store's path
/// of keys (strata names) walked from `new_root` through `listing` (`GET /api/stores`). A store strata has not
/// mirrored is left out.
pub fn mirror_members(
    all: &[StoreRow],
    root: &StoreRow,
    new_root: StrataStoreId,
    listing: &[crate::strata::rag::StoreView],
) -> Map<String, Value> {
    let mut out = Map::new();
    for s in stores::subtree(all, root.id) {
        if s.id == root.id {
            continue;
        }
        let path = stores::path_of(all, s.id);
        let mut cur = Some(new_root);
        for key in path.iter().skip(1) {
            cur = cur.and_then(|p| {
                listing
                    .iter()
                    .find(|v| v.parent_id == Some(p) && v.name == *key)
                    .map(|v| v.id)
            });
        }
        if let Some(sid) = cur {
            out.insert(s.id.to_string(), json!(sid));
        }
    }
    out
}

/// Retire every generation of root store `store` that nothing uses any more (module doc) and queue the deletion of
/// its strata root; the ids retired. Runs in the publishing transaction (crate::knowledge::targets `published`).
pub async fn retire_unpinned(
    c: &mut PgConnection,
    tenant: Uuid,
    store: Uuid,
    actor: &Actor,
) -> ApiResult<Vec<Uuid>> {
    let Some(root) = stores::by_id(&mut *c, store, false).await? else {
        return Ok(Vec::new());
    };
    let gens = of_store(&mut *c, store).await?;
    let newest = gens
        .iter()
        .filter(|g| g.is_ready())
        .max_by_key(|g| g.created_at)
        .map(|g| g.id);
    let mut used: HashSet<Uuid> = HashSet::new();
    let jobs: Vec<(Uuid, String)> = sqlx::query_as("SELECT id, key FROM jobs ORDER BY id")
        .fetch_all(&mut *c)
        .await?;
    for (id, key) in jobs {
        let jr = JobRef { id, key };
        for of in [DesignOf::Draft, DesignOf::Live] {
            if let Some(g) = pinned(&mut *c, &jr, &root, &of).await? {
                used.insert(g.id);
            }
        }
    }
    let open: Vec<Value> = sqlx::query_scalar(
        "SELECT stores FROM model_change_job WHERE state IN ('pending', 'reembedding', 'ready', 'accepted')",
    )
    .fetch_all(&mut *c)
    .await?;
    for entries in &open {
        for e in entries.as_array().map(Vec::as_slice).unwrap_or_default() {
            for k in ["from_generation", "to_generation"] {
                if let Some(id) = e
                    .get(k)
                    .and_then(Value::as_str)
                    .and_then(|s| Uuid::parse_str(s).ok())
                {
                    used.insert(id);
                }
            }
        }
    }
    let mut retired = Vec::new();
    for g in &gens {
        if !g.is_ready() || g.is_initial() || Some(g.id) == newest || used.contains(&g.id) {
            continue;
        }
        let done = sqlx::query(
            "UPDATE store_generation SET state = 'retired', updated_at = clock_timestamp() \
              WHERE id = $1 AND state = 'ready'",
        )
        .bind(g.id)
        .execute(&mut *c)
        .await?;
        if done.rows_affected() == 0 {
            continue;
        }
        if let Some(sid) = g.strata_store_id {
            store_delete::enqueue(&mut *c, tenant, StrataStoreId(sid), actor).await?;
        }
        retired.push(g.id);
    }
    Ok(retired)
}

/// The generations of `ids` by id (missing ids left out).
pub async fn by_ids(c: &mut PgConnection, ids: &[Uuid]) -> ApiResult<HashMap<Uuid, Generation>> {
    let rows: Vec<Generation> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {COLS} FROM store_generation WHERE id = ANY($1)"
    )))
    .bind(ids)
    .fetch_all(&mut *c)
    .await?;
    Ok(rows.into_iter().map(|g| (g.id, g)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rebuild_follows_the_slot() {
        assert_eq!(rebuild_of(Slot::Embed), "reembed");
        assert_eq!(rebuild_of(Slot::Rerank), "copy");
        assert_eq!(rebuild_of(Slot::Chat), "copy");
        for s in [Slot::Ocr, Slot::Vision, Slot::Transcribe] {
            assert_eq!(rebuild_of(s), "reingest");
        }
    }

    #[test]
    fn model_set_replaces_one_slot() {
        let a = ModelSet::default().with(Slot::Embed, Some("e1"));
        let b = a.with(Slot::Rerank, Some("r1"));
        assert_eq!(b.get(Slot::Embed), Some("e1"));
        assert_eq!(b.get(Slot::Rerank), Some("r1"));
        assert_eq!(b.with(Slot::Embed, None).get(Slot::Embed), None);
        assert_ne!(a, b);
        assert_eq!(a.to_value()["embed"], "e1");
        assert_eq!(a.to_value()["ocr"], Value::Null);
    }
}
