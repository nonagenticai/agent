//! B3's tenant-wide design targets (plan §0 R9, D69): the Rust form of the fixture `TARGETS.llm_use`
//! (`web/src/lib/fixtures/ledger.ts`). One line in `design::registry()` ([`register`]).
//!
//! | kind | key | working rows | places |
//! |---|---|---|---|
//! | `llm_use` | `llm-use:<use>` | `llm_use_binding` | every stored binding |
//! | `provider_slot` | `slot:<slot>` | `provider_slot` | every configured slot (T4) |
//!
//! The value of a bound use is `{provider_id, model, provider}` (+ `voice` for a `tts` binding, T4d): `provider` is
//! the provider's name at read time (the fixture's `p?.name ?? u.provider_id`), so it is display only — `proj`
//! compares `[provider_id, model]`. No binding row (or a row without a provider) is `None`: the use runs its fallback.
//!
//! `set` is the restore path (discard, follow-live, a write back) and writes exactly what `get` returns. Handlers
//! write `llm_use_binding` only inside the `mutate` closure of `design::track_tenant`, through [`upsert_binding`] /
//! [`delete_binding`] (tests/design_antidrift.rs: this module is the kind's only other writer).
use std::sync::Arc;

use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use super::kinds::Slot;
use super::slots::{self, SlotDesign};
use crate::design::{DesignTarget, DraftRef, Place, Registry, Scope, Snap, snap_from_json};
use crate::error::{ApiError, ApiResult};
use crate::release::targets::SET_BY;

/// Register B3's provider targets (one line in `design::registry()`).
pub fn register(r: &mut Registry) {
    r.register(Arc::new(LlmUseTarget));
    r.register(Arc::new(ProviderSlotTarget));
}

fn internal(what: String) -> ApiError {
    ApiError::Internal(anyhow::anyhow!(what))
}

/// The `use` of a place ref (`{use}`).
fn use_of(r: &DraftRef) -> &str {
    r.get("use").and_then(Value::as_str).unwrap_or("")
}

/// The ref `{use}` of a use.
pub fn use_ref(use_name: &str) -> DraftRef {
    let mut m = DraftRef::new();
    m.insert("use".to_string(), Value::String(use_name.to_string()));
    m
}

#[derive(sqlx::FromRow)]
struct BindingRow {
    provider_id: Option<Uuid>,
    model: Option<String>,
    voice: Option<String>,
    provider: Option<String>,
}

/// The design value of a bound use (fixture `llm_use.get`).
pub fn binding_value(
    provider_id: Uuid,
    model: Option<&str>,
    voice: Option<&str>,
    provider: Option<&str>,
) -> Value {
    let mut m = Map::new();
    m.insert("provider_id".into(), json!(provider_id.to_string()));
    m.insert("model".into(), json!(model));
    let name = provider.map_or_else(|| provider_id.to_string(), str::to_string);
    m.insert("provider".into(), json!(name));
    if let Some(v) = voice {
        m.insert("voice".into(), json!(v));
    }
    Value::Object(m)
}

/// The provider id of a value, when it names one.
fn bound_provider(v: &Snap) -> Option<&str> {
    v.as_ref()?.get("provider_id")?.as_str()
}

/// Write the working binding of `use_name` (insert or replace). Only inside a `track_tenant` closure or the target's
/// `set`.
pub async fn upsert_binding(
    c: &mut PgConnection,
    tenant: Uuid,
    use_name: &str,
    provider_id: Uuid,
    model: Option<&str>,
    voice: Option<&str>,
    by: &str,
) -> ApiResult<()> {
    sqlx::query(
        "INSERT INTO llm_use_binding (tenant_id, \"use\", provider_id, model, voice, updated_by) \
         VALUES ($1, $2, $3, $4, $5, $6) \
         ON CONFLICT (tenant_id, \"use\") DO UPDATE \
            SET provider_id = EXCLUDED.provider_id, model = EXCLUDED.model, voice = EXCLUDED.voice, \
                updated_by = EXCLUDED.updated_by, updated_at = clock_timestamp()",
    )
    .bind(tenant)
    .bind(use_name)
    .bind(provider_id)
    .bind(model)
    .bind(voice)
    .bind(by)
    .execute(&mut *c)
    .await?;
    Ok(())
}

/// Remove the working binding of `use_name` (the use falls back). Only inside a `track_tenant` closure or `set`.
pub async fn delete_binding(c: &mut PgConnection, tenant: Uuid, use_name: &str) -> ApiResult<()> {
    sqlx::query("DELETE FROM llm_use_binding WHERE tenant_id = $1 AND \"use\" = $2")
        .bind(tenant)
        .bind(use_name)
        .execute(&mut *c)
        .await?;
    Ok(())
}

/// Counter of `llm_use` restores whose provider is deleted or missing ([`LlmUseTarget`]'s `set`).
pub const RESTORED_DANGLING_TOTAL: &str = "agent_llm_use_restored_dangling_total";

/// `llm_use` (D69, D70, D89): which provider + model serves one LLM use of the tenant. Tenant-wide: one working row
/// for every job; each job's Production gets it when that job publishes.
pub struct LlmUseTarget;

impl DesignTarget for LlmUseTarget {
    fn kind(&self) -> &'static str {
        "llm_use"
    }
    fn scope(&self) -> Scope {
        Scope::Tenant
    }
    fn key(&self, r: &DraftRef) -> String {
        format!("llm-use:{}", use_of(r))
    }
    fn get<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
        r: &'a DraftRef,
    ) -> BoxFuture<'a, ApiResult<Snap>> {
        Box::pin(async move {
            let row: Option<BindingRow> = sqlx::query_as(
                "SELECT b.provider_id, b.model, b.voice, p.name AS provider \
                   FROM llm_use_binding b \
                   LEFT JOIN llm_provider p ON p.tenant_id = b.tenant_id AND p.id = b.provider_id \
                  WHERE b.tenant_id = $1 AND b.\"use\" = $2",
            )
            .bind(at.tenant)
            .bind(use_of(r))
            .fetch_optional(&mut *c)
            .await?;
            Ok(row.and_then(|b| {
                b.provider_id.map(|pid| {
                    binding_value(
                        pid,
                        b.model.as_deref(),
                        b.voice.as_deref(),
                        b.provider.as_deref(),
                    )
                })
            }))
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
            let use_name = use_of(r);
            let Some(pid) = bound_provider(v) else {
                return delete_binding(c, at.tenant, use_name).await;
            };
            let pid = Uuid::parse_str(pid)
                .map_err(|_| internal(format!("llm_use provider_id is not a uuid: {pid}")))?;
            let text = |f: &str| -> ApiResult<Option<String>> {
                match v.as_ref().and_then(|x| x.get(f)) {
                    None | Some(Value::Null) => Ok(None),
                    Some(Value::String(s)) => Ok(Some(s.clone())),
                    Some(other) => Err(internal(format!(
                        "llm_use {f} is not a string or null: {other}"
                    ))),
                }
            };
            let model = text("model")?;
            let voice = text("voice")?;
            // a restore (follow-live, a write back) may bring back a binding to a provider deleted since: it is
            // written as it was (the design is what it was), and resolve refuses it loudly (409 provider_deleted)
            let present: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM llm_provider \
                                 WHERE tenant_id = $1 AND id = $2 AND deleted_at IS NULL)",
            )
            .bind(at.tenant)
            .bind(pid)
            .fetch_one(&mut *c)
            .await?;
            if !present {
                metrics::counter!(RESTORED_DANGLING_TOTAL).increment(1);
                tracing::warn!(
                    tenant = %at.tenant,
                    use_ = use_name,
                    provider = %pid,
                    "llm_use restored onto a deleted or missing provider"
                );
            }
            upsert_binding(
                c,
                at.tenant,
                use_name,
                pid,
                model.as_deref(),
                voice.as_deref(),
                SET_BY,
            )
            .await
        })
    }
    fn places<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
    ) -> BoxFuture<'a, ApiResult<Option<Vec<DraftRef>>>> {
        Box::pin(async move {
            let uses: Vec<String> = sqlx::query_scalar(
                "SELECT \"use\" FROM llm_use_binding WHERE tenant_id = $1 ORDER BY \"use\"",
            )
            .bind(at.tenant)
            .fetch_all(&mut *c)
            .await?;
            Ok(Some(uses.iter().map(|u| use_ref(u.as_str())).collect()))
        })
    }
    /// Fixture `proj`: `[provider_id, model]` while bound, else null (the provider's name is display only); a `tts`
    /// binding with a voice adds it (B3 T4d: a voice change is a design change).
    fn proj(&self, v: &Snap) -> Value {
        match bound_provider(v) {
            Some(pid) => {
                let field = |f: &str| {
                    v.as_ref()
                        .and_then(|x| x.get(f))
                        .cloned()
                        .unwrap_or(Value::Null)
                };
                match field("voice") {
                    Value::Null => json!([pid, field("model")]),
                    voice => json!([pid, field("model"), voice]),
                }
            }
            None => Value::Null,
        }
    }
    /// Fixture `text`: `<provider> · <model>`, or `not bound`.
    fn text(&self, v: &Snap, _r: &DraftRef) -> Option<String> {
        let Some(pid) = bound_provider(v) else {
            return Some("not bound".to_string());
        };
        let field = |f: &str| v.as_ref().and_then(|x| x.get(f)).and_then(Value::as_str);
        Some(format!(
            "{} · {}",
            field("provider").unwrap_or(pid),
            field("model").unwrap_or("no model")
        ))
    }
}

// ---- provider_slot (T4) ---------------------------------------------------------------------------------------------

/// The `slot` of a place ref (`{slot}`).
fn slot_of(r: &DraftRef) -> &str {
    r.get("slot").and_then(Value::as_str).unwrap_or("")
}

/// The ref `{slot}` of a slot.
pub fn slot_ref(slot: Slot) -> DraftRef {
    let mut m = DraftRef::new();
    m.insert("slot".to_string(), Value::String(slot.as_str().to_string()));
    m
}

/// Write the working row of `slot`: `Some` = configured with that value, `None` = not configured. Every write bumps
/// `version` (the `expected_version` of the next save); clearing a slot that is not configured changes nothing. Only
/// inside a `track_tenant` closure or the target's `set`.
pub async fn write_slot(
    c: &mut PgConnection,
    tenant: Uuid,
    slot: Slot,
    d: Option<&SlotDesign>,
    by: &str,
) -> ApiResult<()> {
    match d {
        // F-08 (T4b, 0057): the deployment default, configured with nothing of the tenant's
        Some(d) if d.default => {
            sqlx::query(
                "INSERT INTO provider_slot (tenant_id, slot, service, configured, source, version, updated_by) \
                 VALUES ($1, $2, $3, true, 'default', 1, $4) \
                 ON CONFLICT (tenant_id, slot) DO UPDATE \
                    SET configured = true, source = 'default', kind = NULL, base_url = NULL, model = NULL, \
                        secret_id = NULL, key_kept = false, version = provider_slot.version + 1, \
                        updated_at = clock_timestamp(), updated_by = EXCLUDED.updated_by",
            )
            .bind(tenant)
            .bind(slot.as_str())
            .bind(slots::target_of(slot).as_str())
            .bind(by)
            .execute(&mut *c)
            .await?;
        }
        Some(d) => {
            sqlx::query(
                "INSERT INTO provider_slot (tenant_id, slot, service, configured, kind, base_url, model, secret_id, \
                                            key_kept, version, updated_by) \
                 VALUES ($1, $2, $3, true, $4, $5, $6, $7, $8, 1, $9) \
                 ON CONFLICT (tenant_id, slot) DO UPDATE \
                    SET configured = true, source = 'tenant', kind = EXCLUDED.kind, base_url = EXCLUDED.base_url, \
                        model = EXCLUDED.model, secret_id = EXCLUDED.secret_id, key_kept = EXCLUDED.key_kept, \
                        version = provider_slot.version + 1, updated_at = clock_timestamp(), \
                        updated_by = EXCLUDED.updated_by",
            )
            .bind(tenant)
            .bind(slot.as_str())
            .bind(slots::target_of(slot).as_str())
            .bind(&d.kind)
            .bind(&d.base_url)
            .bind(&d.model)
            .bind(d.secret_id)
            .bind(d.key_kept)
            .bind(by)
            .execute(&mut *c)
            .await?;
        }
        None => {
            sqlx::query(
                "UPDATE provider_slot \
                    SET configured = false, source = 'tenant', kind = NULL, base_url = NULL, model = NULL, secret_id = NULL, \
                        key_kept = false, version = version + 1, updated_at = clock_timestamp(), updated_by = $3 \
                  WHERE tenant_id = $1 AND slot = $2 AND configured",
            )
            .bind(tenant)
            .bind(slot.as_str())
            .bind(by)
            .execute(&mut *c)
            .await?;
        }
    }
    Ok(())
}

/// `provider_slot` (D69, plan B3 T4): which provider serves one rag/ingest slot of the tenant. Tenant-wide: one
/// working row for every job; a job's publish pushes the published value to strata ([`DesignTarget::published`] →
/// outbox `connections_put`). The value is `{configured, kind, base_url, model, secret: {set, fingerprint},
/// secret_id}` (fixture `provider_slot.get` + the sealed key's row id); a slot that is not configured is `None`.
pub struct ProviderSlotTarget;

impl DesignTarget for ProviderSlotTarget {
    fn kind(&self) -> &'static str {
        "provider_slot"
    }
    fn scope(&self) -> Scope {
        Scope::Tenant
    }
    fn key(&self, r: &DraftRef) -> String {
        format!("slot:{}", slot_of(r))
    }
    fn get<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
        r: &'a DraftRef,
    ) -> BoxFuture<'a, ApiResult<Snap>> {
        Box::pin(async move {
            let Some(slot) = Slot::parse(slot_of(r)) else {
                return Ok(None);
            };
            let row = slots::row(c, at.tenant, slot, false).await?;
            Ok(row.and_then(|x| x.design()).map(|d| d.to_value()))
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
            let slot = Slot::parse(slot_of(r))
                .ok_or_else(|| internal(format!("provider_slot ref names no slot: {r:?}")))?;
            let d = SlotDesign::from_value(v.as_ref())?;
            write_slot(c, at.tenant, slot, d.as_ref(), SET_BY).await
        })
    }
    fn places<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
    ) -> BoxFuture<'a, ApiResult<Option<Vec<DraftRef>>>> {
        Box::pin(async move {
            let configured: Vec<String> = sqlx::query_scalar(
                "SELECT slot FROM provider_slot WHERE tenant_id = $1 AND configured ORDER BY slot",
            )
            .bind(at.tenant)
            .fetch_all(&mut *c)
            .await?;
            Ok(Some(
                configured
                    .iter()
                    .filter_map(|s| Slot::parse(s.as_str()))
                    .map(slot_ref)
                    .collect(),
            ))
        })
    }
    /// Fixture `proj`: `[configured, kind, base_url, model, secret.set, secret.fingerprint]` (the key's row id is
    /// not compared: the same key saved again is no change).
    fn proj(&self, v: &Snap) -> Value {
        match v.as_ref().filter(|x| !x.is_null()) {
            Some(x) => json!([
                x.get("configured").cloned().unwrap_or(Value::Null),
                x.get("kind").cloned().unwrap_or(Value::Null),
                x.get("base_url").cloned().unwrap_or(Value::Null),
                x.get("model").cloned().unwrap_or(Value::Null),
                x.pointer("/secret/set").cloned().unwrap_or(Value::Null),
                x.pointer("/secret/fingerprint")
                    .cloned()
                    .unwrap_or(Value::Null),
            ]),
            None => Value::Null,
        }
    }
    /// Fixture `text`: `<kind> · <model> @ <host>`, or `not configured`.
    fn text(&self, v: &Snap, _r: &DraftRef) -> Option<String> {
        let Ok(Some(d)) = SlotDesign::from_value(v.as_ref()) else {
            return Some("not configured".to_string());
        };
        if d.default {
            return Some("deployment default".to_string());
        }
        let at = d
            .base_url
            .as_deref()
            .map(|u| format!(" @ {}", slots::host_of(u)))
            .unwrap_or_default();
        Some(format!("{} \u{b7} {}{at}", d.kind, d.model))
    }
    /// A job published a release bundling this slot's change: queue the push of the value that release holds
    /// (outbox `connections_put`, sent by crate::provision::senders::connections).
    fn published<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
        r: &'a DraftRef,
        release: &'a str,
    ) -> BoxFuture<'a, ApiResult<()>> {
        Box::pin(async move {
            let (Some(job), Some(slot)) = (at.job, Slot::parse(slot_of(r))) else {
                return Ok(());
            };
            let stored: Option<Option<Value>> = sqlx::query_scalar(
                "SELECT snap FROM release_snapshot \
                  WHERE tenant_id = $1 AND job_id = $2 AND release_id = $3 AND key = $4",
            )
            .bind(at.tenant)
            .bind(job.id)
            .bind(release)
            .bind(format!("slot:{}", slot.as_str()))
            .fetch_optional(&mut *c)
            .await?;
            let value = snap_from_json(stored.flatten())?;
            let d = SlotDesign::from_value(value.as_ref())?;
            let version: Option<i32> = sqlx::query_scalar(
                "SELECT version FROM provider_slot WHERE tenant_id = $1 AND slot = $2",
            )
            .bind(at.tenant)
            .bind(slot.as_str())
            .fetch_optional(&mut *c)
            .await?;
            slots::enqueue_push(
                c,
                at.tenant,
                slot,
                d.as_ref(),
                version.unwrap_or(0),
                Some(release),
            )
            .await?;
            Ok(())
        })
    }
}
