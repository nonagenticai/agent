//! The scope registry: the SET of scopes the runtime serves, at the environment root (`{prefix}scopes`), with a
//! Pub/Sub nudge (`{prefix}scopes:changed`) on every change. Ported from care-bus-agents@96c608c
//! `crates/carebus/src/scopes.rs`; members are `<tenant>/<job>/<env>` (B4 R5).
//!
//! agent-api maintains it from the database: [`reconcile`] (add-only; a scope leaves only after two passes without
//! it, [`apply_pass`]) runs at boot from `agent_all_scopes()` (every job
//! of every active tenant × {prod, test}, migration 0065) and again from [`spawn_reconciler`] (retried with backoff
//! until Redis accepts it, then every [`RECONCILE_EVERY`]), and [`changed`] re-derives one tenant's scopes after a
//! tenant-status write or a job create commits. The supervisor in every runtime process follows it.

use std::collections::BTreeSet;
use std::time::Duration;

use anyhow::{Context, Result};
use redis::AsyncCommands;
use sqlx::PgPool;
use uuid::Uuid;

use super::bus::Bus;
use super::scope::{Env, Scope};
use crate::state::AppState;

/// `root_key` → SET of `<tenant>/<job>/<env>`.
pub const SCOPES_KEY: &str = "scopes";
/// `root_key` → Pub/Sub channel; the payload is ignored (subscribers re-read the SET).
pub const SCOPES_CHANNEL: &str = "scopes:changed";

/// Every registered scope, sorted. Unparseable members are logged and skipped.
pub async fn list(root: &Bus) -> Result<Vec<Scope>> {
    let key = root.root_key(SCOPES_KEY);
    let mut c = root.conn();
    let members: Vec<String> = c
        .smembers(&key)
        .await
        .with_context(|| format!("SMEMBERS {key}"))?;
    let mut all: Vec<Scope> = members
        .iter()
        .filter_map(|m| match Scope::parse(m) {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::warn!(member = %m, key = %key, "skipping unparseable scope: {e:#}");
                None
            }
        })
        .collect();
    all.sort_by_key(|s| s.to_string());
    Ok(all)
}

/// Register `s` and nudge the supervisors.
pub async fn add(root: &Bus, s: &Scope) -> Result<()> {
    let key = root.root_key(SCOPES_KEY);
    let mut c = root.conn();
    let _: i64 = c
        .sadd(&key, s.to_string())
        .await
        .with_context(|| format!("SADD {key} {s}"))?;
    notify(root).await
}

/// Unregister `s` and nudge the supervisors.
pub async fn remove(root: &Bus, s: &Scope) -> Result<()> {
    let key = root.root_key(SCOPES_KEY);
    let mut c = root.conn();
    let _: i64 = c
        .srem(&key, s.to_string())
        .await
        .with_context(|| format!("SREM {key} {s}"))?;
    notify(root).await
}

/// Replace the whole registry atomically (MULTI: build a temp SET, RENAME it over the registry), then nudge. An
/// empty `all` deletes the registry.
pub async fn replace_all(root: &Bus, all: &[Scope]) -> Result<()> {
    let key = root.root_key(SCOPES_KEY);
    let mut members: Vec<String> = all.iter().map(Scope::to_string).collect();
    members.sort();
    members.dedup();
    let mut pipe = redis::pipe();
    pipe.atomic();
    if members.is_empty() {
        pipe.cmd("DEL").arg(&key).ignore();
    } else {
        let tmp = root.root_key(&format!("{SCOPES_KEY}:tmp:{}", Uuid::new_v4().simple()));
        pipe.cmd("DEL").arg(&tmp).ignore();
        pipe.cmd("SADD").arg(&tmp).arg(&members).ignore();
        pipe.cmd("RENAME").arg(&tmp).arg(&key).ignore();
    }
    let mut c = root.conn();
    pipe.query_async::<()>(&mut c)
        .await
        .with_context(|| format!("replace {key} with {} scopes", members.len()))?;
    notify(root).await
}

/// Make the registry members of `tenant` exactly `want` (other tenants' members are untouched), nudging only
/// when something changed. Returns whether it did.
pub async fn set_tenant(root: &Bus, tenant: Uuid, want: &[Scope]) -> Result<bool> {
    let key = root.root_key(SCOPES_KEY);
    let mut c = root.conn();
    let members: Vec<String> = c
        .smembers(&key)
        .await
        .with_context(|| format!("SMEMBERS {key}"))?;
    let mine = format!("{}/", tenant.hyphenated());
    let have: BTreeSet<String> = members
        .into_iter()
        .filter(|m| m.starts_with(&mine))
        .collect();
    let want: BTreeSet<String> = want
        .iter()
        .filter(|s| s.tenant == tenant)
        .map(Scope::to_string)
        .collect();
    let add: Vec<String> = want.difference(&have).cloned().collect();
    let del: Vec<String> = have.difference(&want).cloned().collect();
    if add.is_empty() && del.is_empty() {
        return Ok(false);
    }
    let mut pipe = redis::pipe();
    pipe.atomic();
    if !add.is_empty() {
        pipe.cmd("SADD").arg(&key).arg(&add).ignore();
    }
    if !del.is_empty() {
        pipe.cmd("SREM").arg(&key).arg(&del).ignore();
    }
    pipe.query_async::<()>(&mut c)
        .await
        .with_context(|| format!("update {key} for tenant {tenant}"))?;
    notify(root).await?;
    Ok(true)
}

async fn notify(root: &Bus) -> Result<()> {
    let channel = root.root_key(SCOPES_CHANNEL);
    let mut c = root.conn();
    let _: i64 = c
        .publish(&channel, "changed")
        .await
        .with_context(|| format!("PUBLISH {channel}"))?;
    Ok(())
}

/// Every scope the database says the runtime serves: the jobs of active tenants × {prod, test}, from the
/// SECURITY DEFINER `agent_all_scopes()` (0065; agent-api reads it before any tenant is known).
pub async fn all_from_db(db: &PgPool) -> Result<Vec<Scope>> {
    let rows: Vec<(Uuid, String, String)> =
        sqlx::query_as("SELECT tenant_id, job_key, env FROM agent_all_scopes()")
            .fetch_all(db)
            .await
            .context("read the served scopes (agent_all_scopes)")?;
    Ok(rows
        .into_iter()
        .filter_map(|(tenant, job, env)| {
            let env = Env::parse(&env)?;
            Scope::new(tenant, job, env)
                .map_err(|e| tracing::warn!("skipping an unservable scope: {e:#}"))
                .ok()
        })
        .collect())
}

/// The scopes of one tenant: its jobs × {prod, test} while it is `active`, none otherwise. Read as the tenant
/// (row-level security applies).
pub async fn tenant_from_db(db: &PgPool, tenant: Uuid) -> Result<Vec<Scope>> {
    let mut tx = crate::tenancy::begin_scoped(db, tenant).await?;
    let status: Option<String> = sqlx::query_scalar("SELECT status FROM tenants WHERE id = $1")
        .bind(tenant)
        .fetch_optional(&mut *tx)
        .await
        .context("read the tenant's status")?;
    let keys: Vec<String> = if status.as_deref() == Some("active") {
        sqlx::query_scalar("SELECT key FROM jobs WHERE tenant_id = $1 ORDER BY key")
            .bind(tenant)
            .fetch_all(&mut *tx)
            .await
            .context("read the tenant's jobs")?
    } else {
        Vec::new()
    };
    tx.commit().await?;
    let mut out = Vec::with_capacity(keys.len() * Env::ALL.len());
    for key in keys {
        for env in Env::ALL {
            match Scope::new(tenant, key.clone(), env) {
                Ok(s) => out.push(s),
                Err(e) => tracing::warn!("skipping an unservable scope: {e:#}"),
            }
        }
    }
    Ok(out)
}

/// `root_key` → HASH of registered scope → how many consecutive [`reconcile`] passes found it missing from the
/// database ([`apply_pass`]).
pub const SCOPES_MISSING_KEY: &str = "scopes:missing";
/// A registered scope is removed by [`reconcile`] only after this many consecutive passes without it in the database.
pub const MISSES_TO_REMOVE: i64 = 2;

/// What one [`apply_pass`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pass {
    /// Scopes the database names (all registered after the pass).
    pub wanted: usize,
    pub added: usize,
    pub removed: usize,
}

/// Bring the registry toward the database, so the runtime serves the provisioned scopes even after Redis was
/// flushed or a registry write after a commit failed. Returns how many scopes the database names.
pub async fn reconcile(db: &PgPool, root: &Bus) -> Result<usize> {
    let all = all_from_db(db).await?;
    let p = apply_pass(root, &all)
        .await
        .context("reconcile the scope registry")?;
    Ok(p.wanted)
}

/// One reconcile pass over `want` (the database's scopes, read before this call). ADD-ONLY for what the database
/// names (SADD): a scope a concurrent job create registered after `want` was read is never dropped. A registered
/// scope the database does not name is removed only once it has been missing [`MISSES_TO_REMOVE`] consecutive
/// passes (counted in [`SCOPES_MISSING_KEY`], reset as soon as a pass sees it). Nudges only on a real add/remove.
pub async fn apply_pass(root: &Bus, want: &[Scope]) -> Result<Pass> {
    let key = root.root_key(SCOPES_KEY);
    let missing = root.root_key(SCOPES_MISSING_KEY);
    let want: BTreeSet<String> = want.iter().map(Scope::to_string).collect();
    let mut c = root.conn();
    let mut added = 0usize;
    if !want.is_empty() {
        let names: Vec<String> = want.iter().cloned().collect();
        let n: i64 = c
            .sadd(&key, &names)
            .await
            .with_context(|| format!("SADD {key}"))?;
        added = usize::try_from(n).unwrap_or_default();
        let _: i64 = c
            .hdel(&missing, &names)
            .await
            .with_context(|| format!("HDEL {missing}"))?;
    }
    let registered: Vec<String> = c
        .smembers(&key)
        .await
        .with_context(|| format!("SMEMBERS {key}"))?;
    let members: BTreeSet<String> = registered.into_iter().collect();
    let mut gone = Vec::new();
    for m in members.difference(&want) {
        let misses: i64 = c
            .hincr(&missing, m, 1)
            .await
            .with_context(|| format!("HINCRBY {missing}"))?;
        if misses >= MISSES_TO_REMOVE {
            gone.push(m.clone());
        }
    }
    // counters of scopes no longer registered (removed elsewhere) are dropped too
    let counted: Vec<String> = c
        .hkeys(&missing)
        .await
        .with_context(|| format!("HKEYS {missing}"))?;
    let mut stale: Vec<String> = counted
        .into_iter()
        .filter(|m| !members.contains(m))
        .collect();
    let mut removed = 0usize;
    if !gone.is_empty() {
        let n: i64 = c
            .srem(&key, &gone)
            .await
            .with_context(|| format!("SREM {key}"))?;
        removed = usize::try_from(n).unwrap_or_default();
        stale.extend(gone);
    }
    if !stale.is_empty() {
        let _: i64 = c
            .hdel(&missing, &stale)
            .await
            .with_context(|| format!("HDEL {missing}"))?;
    }
    if added > 0 || removed > 0 {
        notify(root).await?;
    }
    Ok(Pass {
        wanted: want.len(),
        added,
        removed,
    })
}

/// How often [`spawn_reconciler`] re-runs a successful [`reconcile`] (repairs a flushed Redis or a lost
/// [`changed`] write; a deleted scope leaves after two passes).
pub const RECONCILE_EVERY: Duration = Duration::from_secs(300);
/// The first retry after a failed [`reconcile`]; doubled per failure up to [`RECONCILE_BACKOFF_MAX`].
pub const RECONCILE_BACKOFF_MIN: Duration = Duration::from_secs(1);
/// The longest wait between two failed attempts.
pub const RECONCILE_BACKOFF_MAX: Duration = Duration::from_secs(60);
/// One attempt's bound, so an unreachable Redis can neither hold the boot nor wedge the loop.
const RECONCILE_ATTEMPT: Duration = Duration::from_secs(10);

/// One bounded [`reconcile`]: `true` when the registry was rebuilt. Logs and counts a failure, never fails.
pub async fn reconcile_once(db: &PgPool, root: &Bus) -> bool {
    match tokio::time::timeout(RECONCILE_ATTEMPT, reconcile(db, root)).await {
        Ok(Ok(n)) => {
            tracing::info!(scopes = n, "scope registry rebuilt from the database");
            true
        }
        Ok(Err(e)) => {
            metrics::counter!("agent_scope_registry_errors_total").increment(1);
            tracing::warn!("scope registry rebuild failed: {e:#}");
            false
        }
        Err(_) => {
            metrics::counter!("agent_scope_registry_errors_total").increment(1);
            tracing::warn!("scope registry rebuild timed out (is Redis reachable?)");
            false
        }
    }
}

/// Keep the registry reconciled (agent-api, while `REDIS_URL` is set), after the boot rebuild: rebuild again after
/// [`RECONCILE_BACKOFF_MIN`]; while a rebuild fails (Redis not accepting, as when agent-api booted before its Redis)
/// retry with backoff (doubling to [`RECONCILE_BACKOFF_MAX`]) until it succeeds; after a success, rebuild every
/// `every`. Idempotent (the SET is replaced as a whole from the database). Never calls an LLM.
pub fn spawn_reconciler(db: PgPool, root: Bus, every: Duration) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut backoff = RECONCILE_BACKOFF_MIN;
        let mut wait = backoff;
        loop {
            tokio::time::sleep(wait).await;
            if reconcile_once(&db, &root).await {
                backoff = RECONCILE_BACKOFF_MIN;
                wait = every;
            } else {
                wait = backoff;
                backoff = (backoff * 2).min(RECONCILE_BACKOFF_MAX);
            }
        }
    })
}

/// After a tenant-status write or a job create COMMITTED: re-derive `tenant`'s scopes into the registry. Never
/// fails the caller (the change is committed): a Redis or read error is logged and counted, and the next
/// [`spawn_reconciler`] pass repairs the registry. A no-op while `REDIS_URL` is unset.
pub async fn changed(st: &AppState, tenant: Uuid) {
    let Some(root) = st.redis.as_ref() else {
        return;
    };
    let r = async {
        let want = tenant_from_db(&st.db, tenant).await?;
        set_tenant(root, tenant, &want).await
    }
    .await;
    if let Err(e) = r {
        metrics::counter!("agent_scope_registry_errors_total").increment(1);
        tracing::warn!(%tenant, "scope registry update failed (repaired by the next reconcile): {e:#}");
    }
}
