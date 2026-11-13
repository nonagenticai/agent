//! B3's `embedding_pin` design target (T4a; D83/D93, plan §0 R9, B2 §2.5 row 17a): which generation (model set) of
//! a root knowledge store a job uses. One line in `design::registry()` ([`register`]). T10's `knowledge` target lives
//! in crate::knowledge::kinds, not here.
//!
//! | kind | key | working rows | places |
//! |---|---|---|---|
//! | `embedding_pin` | `embedding:<store_key>` | `job_embedding_pin` | every pinned store of the job |
//!
//! The value is `{store, generation, models: {embed, rerank, chat, ocr, vision, transcribe}}`; `proj` compares the
//! generation only (the models are display). No pin is `None`: the job uses the store's `initial` generation
//! (crate::knowledge::generations). Handlers write `job_embedding_pin` only inside the `mutate` closure of
//! `design::track` (acceptModelChange), through [`write_pin`] / [`delete_pin`] (tests/design_antidrift.rs: this module
//! is the kind's only other writer). `published` marks the job's accepted model changes `published` and retires the
//! generations nothing uses any more.
use std::sync::Arc;

use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::design::{DesignTarget, DraftRef, Place, Registry, Scope, Snap};
use crate::error::{ApiError, ApiResult};
use crate::release::targets::SET_BY;

/// Register B3's knowledge targets owned by T4a (one line in `design::registry()`).
pub fn register(r: &mut Registry) {
    r.register(Arc::new(EmbeddingPinTarget));
}

fn internal(what: String) -> ApiError {
    ApiError::Internal(anyhow::anyhow!(what))
}

/// The `store` (root store key) of a place ref.
fn store_of(r: &DraftRef) -> &str {
    r.get("store").and_then(Value::as_str).unwrap_or("")
}

/// The ref `{store}` of a root store.
pub fn store_ref(key: &str) -> DraftRef {
    let mut m = DraftRef::new();
    m.insert("store".to_string(), Value::String(key.to_string()));
    m
}

/// The design value of a pin.
pub fn pin_value(store: &str, generation: Uuid, models: &Value) -> Value {
    json!({"store": store, "generation": generation.to_string(), "models": models})
}

/// Write the working pin of `store_key` for `job` (insert or replace). Only inside a `track` closure or `set`.
pub async fn write_pin(
    c: &mut PgConnection,
    tenant: Uuid,
    job: Uuid,
    store_key: &str,
    generation: Uuid,
    models: &Value,
    by: &str,
) -> ApiResult<()> {
    sqlx::query(
        "INSERT INTO job_embedding_pin (tenant_id, job_id, store_key, generation_id, models, updated_by) \
         VALUES ($1, $2, $3, $4, $5, $6) \
         ON CONFLICT (tenant_id, job_id, store_key) DO UPDATE \
            SET generation_id = EXCLUDED.generation_id, models = EXCLUDED.models, \
                updated_by = EXCLUDED.updated_by, updated_at = clock_timestamp()",
    )
    .bind(tenant)
    .bind(job)
    .bind(store_key)
    .bind(generation)
    .bind(models)
    .bind(by)
    .execute(&mut *c)
    .await?;
    Ok(())
}

/// Remove the working pin (the job uses the store's `initial` generation). Only inside a `track` closure or `set`.
pub async fn delete_pin(
    c: &mut PgConnection,
    tenant: Uuid,
    job: Uuid,
    store_key: &str,
) -> ApiResult<()> {
    sqlx::query(
        "DELETE FROM job_embedding_pin WHERE tenant_id = $1 AND job_id = $2 AND store_key = $3",
    )
    .bind(tenant)
    .bind(job)
    .bind(store_key)
    .execute(&mut *c)
    .await?;
    Ok(())
}

/// The generation id of a value (`None` for nothing at the place).
fn generation_of(v: &Snap) -> Option<&str> {
    v.as_ref()?.get("generation")?.as_str()
}

/// `embedding_pin` (D83/D93): the model set a job's root store runs. Job-scoped: each job pins its own, and a job's
/// Production gets a new pin only when that job publishes it.
pub struct EmbeddingPinTarget;

impl DesignTarget for EmbeddingPinTarget {
    fn kind(&self) -> &'static str {
        "embedding_pin"
    }
    fn scope(&self) -> Scope {
        Scope::Job
    }
    fn key(&self, r: &DraftRef) -> String {
        format!("embedding:{}", store_of(r))
    }
    fn get<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
        r: &'a DraftRef,
    ) -> BoxFuture<'a, ApiResult<Snap>> {
        Box::pin(async move {
            let Some(job) = at.job else {
                return Ok(None);
            };
            let row: Option<(Uuid, Value)> = sqlx::query_as(
                "SELECT generation_id, models FROM job_embedding_pin \
                  WHERE tenant_id = $1 AND job_id = $2 AND store_key = $3",
            )
            .bind(at.tenant)
            .bind(job.id)
            .bind(store_of(r))
            .fetch_optional(&mut *c)
            .await?;
            Ok(row.map(|(g, m)| pin_value(store_of(r), g, &m)))
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
            let job = at
                .job
                .ok_or_else(|| internal("embedding_pin is written per job".to_string()))?;
            let store = store_of(r);
            let Some(g) = generation_of(v) else {
                return delete_pin(c, at.tenant, job.id, store).await;
            };
            let g = Uuid::parse_str(g)
                .map_err(|_| internal(format!("embedding_pin generation is not a uuid: {g}")))?;
            let models = match v.as_ref().and_then(|x| x.get("models")) {
                Some(m @ Value::Object(_)) => m.clone(),
                None | Some(Value::Null) => json!({}),
                Some(other) => {
                    return Err(internal(format!(
                        "embedding_pin models is not an object: {other}"
                    )));
                }
            };
            write_pin(c, at.tenant, job.id, store, g, &models, SET_BY).await
        })
    }
    fn places<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
    ) -> BoxFuture<'a, ApiResult<Option<Vec<DraftRef>>>> {
        Box::pin(async move {
            let Some(job) = at.job else {
                return Ok(Some(Vec::new()));
            };
            let keys: Vec<String> = sqlx::query_scalar(
                "SELECT store_key FROM job_embedding_pin WHERE tenant_id = $1 AND job_id = $2 ORDER BY store_key",
            )
            .bind(at.tenant)
            .bind(job.id)
            .fetch_all(&mut *c)
            .await?;
            Ok(Some(keys.iter().map(|k| store_ref(k)).collect()))
        })
    }
    /// Fixture `proj`: the generation (the models are display), or null.
    fn proj(&self, v: &Snap) -> Value {
        generation_of(v).map_or(Value::Null, |g| json!(g))
    }
    /// Fixture `text`: `<embed model> · generation <first 8>`, or `first generation`.
    fn text(&self, v: &Snap, _r: &DraftRef) -> Option<String> {
        let Some(g) = generation_of(v) else {
            return Some("first generation".to_string());
        };
        let embed = v
            .as_ref()
            .and_then(|x| x.pointer("/models/embed"))
            .and_then(Value::as_str)
            .unwrap_or("default");
        let short: String = g.chars().take(8).collect();
        Some(format!("{embed} \u{b7} generation {short}"))
    }
    /// A job published a release bundling this pin: its accepted model changes are `published`, and generations of
    /// the store nothing uses any more are retired (outbox `store_delete`).
    fn published<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
        r: &'a DraftRef,
        _release: &'a str,
    ) -> BoxFuture<'a, ApiResult<()>> {
        Box::pin(async move {
            let Some(job) = at.job else {
                return Ok(());
            };
            super::model_changes::on_published(c, at.tenant, job, store_of(r)).await
        })
    }
}
