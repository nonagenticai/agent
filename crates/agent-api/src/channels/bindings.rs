//! Phase-A bindings (B11, no table): which provider-side workspace answers as which job, from one JSON env var per
//! kind, e.g. `WEBCHAT_BINDINGS='[{"external_id":"<widget key>","tenant":"<uuid>","job_id":"<uuid>","job_key":"support"}]'`.
//! `external_id` is the web chat widget key or a provider workspace id.
//! Phase B replaces this with `0120_connector_bindings` and a config screen. The interim backend still serves only
//! its allow-listed test tenants (D102), so a binding to any other tenant answers "not available".
//!
//! B12: [`BindingStore`] holds the env bindings ∪ the core bindings (`GET /api/internal/channel/bindings`, pulled at
//! boot, retried with backoff until core answers, then every [`REFRESH_EVERY`]); an env binding wins a
//! `(kind, side, external_id)` clash, so phase A behaves exactly as before. A failed refresh keeps the last good set.
//! A lookup miss may pull once more right away ([`BindingStore::find_or_refresh`], at most one per
//! [`MISS_REFRESH_EVERY`]), so a binding created in the console answers within seconds, not after the next period.
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use tokio::time::Instant;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;
use uuid::Uuid;

use super::connector::{ChannelKind, Side};
use super::core::{CoreBinding, CoreError, CoreLink};
use crate::interaction::model::Scope;

/// How often the edge re-reads the core bindings.
pub const REFRESH_EVERY: Duration = Duration::from_secs(60);
/// Until the first pull succeeds (agent-api may boot after the edge) it is retried after 1 s, 2 s, 4 s … capped here.
pub const FIRST_RETRY: Duration = Duration::from_secs(1);
pub const RETRY_CAP: Duration = Duration::from_secs(30);
/// At most one miss-triggered pull per this window, so random keys cannot hammer core.
pub const MISS_REFRESH_EVERY: Duration = Duration::from_secs(5);

/// One provider-side workspace (or worker target) bound to a job.
#[derive(Clone, PartialEq)]
pub struct Binding {
    /// The `connector_bindings` id; `None` for an env (phase-A) binding.
    pub id: Option<Uuid>,
    pub kind: ChannelKind,
    pub side: Side,
    pub external_id: String,
    pub scope: Scope,
    /// `prod` | `test`.
    pub env: String,
    /// Non-secret settings (web chat `allowed_origins`, worker targets).
    pub settings: Value,
    /// A BYO credential overriding the platform's env credential for this binding. Never logged.
    pub credential: Option<BTreeMap<String, String>>,
}

impl fmt::Debug for Binding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Binding")
            .field("id", &self.id)
            .field("kind", &self.kind)
            .field("side", &self.side)
            .field("scope", &self.scope)
            .field("env", &self.env)
            .field(
                "credential",
                &self.credential.as_ref().map(|_| "<redacted>"),
            )
            .finish_non_exhaustive()
    }
}

impl Binding {
    /// A phase-A env binding (customer side, prod, no settings).
    pub fn from_env(kind: ChannelKind, external_id: impl Into<String>, scope: Scope) -> Self {
        Self {
            id: None,
            kind,
            side: Side::Customer,
            external_id: external_id.into(),
            scope,
            env: "prod".into(),
            settings: Value::Null,
            credential: None,
        }
    }

    /// `None` for a kind this edge has no connector type for (e.g. a kind added on agent-api first).
    pub fn from_core(b: &CoreBinding) -> Option<Self> {
        Some(Self {
            id: Some(b.id),
            kind: ChannelKind::parse(&b.kind)?,
            side: b.side,
            external_id: b.external_id.clone(),
            scope: Scope {
                tenant: b.tenant,
                job_id: b.job_id,
                job_key: b.job_key.clone(),
            },
            env: b.env.clone(),
            settings: b.settings.clone(),
            credential: b.credential.clone(),
        })
    }

    /// Web chat: the binding's own `settings.allowed_origins` (lowercase, no trailing slash).
    pub fn allowed_origins(&self) -> Vec<String> {
        self.settings
            .get("allowed_origins")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .filter_map(super::webchat::normalize_origin)
                    .collect()
            })
            .unwrap_or_default()
    }
}

type Key = (ChannelKind, Side, String);

#[derive(Default)]
struct Index {
    by_key: HashMap<Key, Binding>,
    by_id: HashMap<Uuid, Binding>,
    core: usize,
}

/// Env bindings ∪ core bindings, swapped whole on every refresh (readers never see a half-built set).
pub struct BindingStore {
    env: Vec<Binding>,
    current: RwLock<Arc<Index>>,
    /// When the last miss-triggered pull started; held across that pull, so concurrent misses wait for it and then
    /// read the fresh set instead of pulling again.
    last_miss_refresh: tokio::sync::Mutex<Option<Instant>>,
}

impl Default for BindingStore {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

impl BindingStore {
    pub fn new(env: Vec<Binding>) -> Self {
        let s = Self {
            env,
            current: RwLock::new(Arc::new(Index::default())),
            last_miss_refresh: tokio::sync::Mutex::new(None),
        };
        s.replace_core(Vec::new());
        s
    }

    fn snapshot(&self) -> Arc<Index> {
        self.current
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Replace the core half; the env half stays and wins any clash.
    pub fn replace_core(&self, core: Vec<Binding>) {
        let mut idx = Index::default();
        for b in core {
            let key = (b.kind, b.side, b.external_id.clone());
            if let Some(id) = b.id {
                idx.by_id.insert(id, b.clone());
            }
            idx.by_key.insert(key, b);
            idx.core += 1;
        }
        for b in &self.env {
            idx.by_key
                .insert((b.kind, b.side, b.external_id.clone()), b.clone());
        }
        *self.current.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(idx);
    }

    pub fn find(&self, kind: ChannelKind, side: Side, external_id: &str) -> Option<Binding> {
        self.snapshot()
            .by_key
            .get(&(kind, side, external_id.to_string()))
            .cloned()
    }

    /// [`BindingStore::find`]; on a miss, one immediate pull from core (unless one started less than
    /// [`MISS_REFRESH_EVERY`] ago), then the lookup again. A failed pull keeps the last good set and is a miss.
    pub async fn find_or_refresh(
        &self,
        kind: ChannelKind,
        side: Side,
        external_id: &str,
        core: &CoreLink,
    ) -> Option<Binding> {
        if let Some(b) = self.find(kind, side, external_id) {
            return Some(b);
        }
        let mut last = self.last_miss_refresh.lock().await;
        // a pull that finished while this miss waited for the lock may already hold it
        if let Some(b) = self.find(kind, side, external_id) {
            return Some(b);
        }
        let now = Instant::now();
        if last.is_some_and(|t| now.duration_since(t) < MISS_REFRESH_EVERY) {
            return None;
        }
        *last = Some(now);
        if let Err(e) = self.refresh(core).await {
            tracing::warn!(error = %e, "edge_bindings_miss_refresh_failed");
            return None;
        }
        drop(last);
        self.find(kind, side, external_id)
    }

    pub fn by_id(&self, id: Uuid) -> Option<Binding> {
        self.snapshot().by_id.get(&id).cloned()
    }

    /// Every binding of one kind and side.
    pub fn of_kind(&self, kind: ChannelKind, side: Side) -> Vec<Binding> {
        self.snapshot()
            .by_key
            .values()
            .filter(|b| b.kind == kind && b.side == side)
            .cloned()
            .collect()
    }

    pub fn len(&self) -> usize {
        self.snapshot().by_key.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Core bindings in the current set.
    pub fn core_len(&self) -> usize {
        self.snapshot().core
    }

    /// One pull from core; on failure the last good set stays.
    pub async fn refresh(&self, core: &CoreLink) -> Result<usize, CoreError> {
        let rows = core.bindings().await?;
        let bindings: Vec<Binding> = rows.iter().filter_map(Binding::from_core).collect();
        let n = bindings.len();
        self.replace_core(bindings);
        Ok(n)
    }

    /// Pull now, retrying with backoff ([`FIRST_RETRY`] doubling to [`RETRY_CAP`]) until core answers, then every
    /// `every` forever; one warning per failure streak, one info when it recovers.
    pub fn spawn_refresh(
        self: Arc<Self>,
        core: Arc<CoreLink>,
        every: Duration,
    ) -> tokio::task::JoinHandle<()> {
        self.spawn_refresh_with(core, every, FIRST_RETRY, RETRY_CAP)
    }

    /// [`BindingStore::spawn_refresh`] with an explicit startup backoff (tests use milliseconds).
    pub fn spawn_refresh_with(
        self: Arc<Self>,
        core: Arc<CoreLink>,
        every: Duration,
        first_retry: Duration,
        retry_cap: Duration,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            // Startup: until the first success the edge has only its env bindings, so retry fast, not per period.
            let mut failing = false;
            let mut delay = first_retry;
            loop {
                match self.refresh(&core).await {
                    Ok(n) => {
                        if failing {
                            tracing::info!(bindings = n, "edge_bindings_refresh_recovered");
                        }
                        tracing::info!(bindings = n, "edge_core_bindings_loaded");
                        break;
                    }
                    Err(e) => {
                        if !failing {
                            tracing::warn!(error = %e, "edge_bindings_refresh_failed");
                        }
                        failing = true;
                        tokio::time::sleep(delay).await;
                        delay = (delay * 2).min(retry_cap);
                    }
                }
            }
            failing = false;
            let mut tick = tokio::time::interval(every);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            tick.tick().await;
            loop {
                tick.tick().await;
                match self.refresh(&core).await {
                    Ok(n) => {
                        if failing {
                            tracing::info!(bindings = n, "edge_bindings_refresh_recovered");
                        }
                        failing = false;
                    }
                    Err(e) => {
                        if !failing {
                            tracing::warn!(error = %e, "edge_bindings_refresh_failed");
                        }
                        failing = true;
                    }
                }
            }
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Row {
    external_id: String,
    tenant: Uuid,
    job_id: Uuid,
    job_key: String,
}

/// `external_id` → [`Scope`]. Unset or blank is no bindings; malformed JSON, a blank `external_id` or `job_key`, or a
/// duplicate `external_id` fails the boot by the variable's name (never echoing its value).
pub fn parse(var: &str, raw: Option<&str>) -> Result<BTreeMap<String, Scope>> {
    let Some(raw) = raw.map(str::trim).filter(|r| !r.is_empty()) else {
        return Ok(BTreeMap::new());
    };
    let rows: Vec<Row> = serde_json::from_str(raw).ok().with_context(|| {
        format!("{var} must be a JSON array of {{external_id, tenant, job_id, job_key}}")
    })?;
    let mut out = BTreeMap::new();
    for r in rows {
        let ext = r.external_id.trim().to_string();
        if ext.is_empty() || r.job_key.trim().is_empty() {
            anyhow::bail!("{var}: external_id and job_key must not be blank");
        }
        let scope = Scope {
            tenant: r.tenant,
            job_id: r.job_id,
            job_key: r.job_key.trim().to_string(),
        };
        if out.insert(ext, scope).is_some() {
            anyhow::bail!("{var}: an external_id is bound twice");
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(k: &str) -> Scope {
        Scope {
            tenant: Uuid::nil(),
            job_id: Uuid::nil(),
            job_key: k.into(),
        }
    }

    #[test]
    fn env_wins_and_core_is_swapped_whole() {
        let store = BindingStore::new(vec![Binding::from_env(
            ChannelKind::Telegram,
            "T1",
            scope("env"),
        )]);
        let core = |ext: &str, k: &str| Binding {
            id: Some(Uuid::new_v4()),
            ..Binding::from_env(ChannelKind::Telegram, ext, scope(k))
        };
        let (a, b) = (core("T1", "core"), core("T2", "core"));
        store.replace_core(vec![a.clone(), b.clone()]);
        let hit = store
            .find(ChannelKind::Telegram, Side::Customer, "T1")
            .unwrap();
        assert_eq!(hit.scope.job_key, "env");
        assert_eq!(
            store.by_id(b.id.unwrap()).unwrap().external_id,
            "T2".to_string()
        );
        assert_eq!(store.core_len(), 2);
        store.replace_core(vec![]);
        assert!(
            store
                .find(ChannelKind::Telegram, Side::Customer, "T2")
                .is_none()
        );
        assert!(
            store
                .find(ChannelKind::Telegram, Side::Customer, "T1")
                .is_some()
        );
        assert!(
            store
                .find(ChannelKind::Telegram, Side::Worker, "T1")
                .is_none()
        );
    }

    const T: &str = "d431fc63-0000-4000-8000-000000000001";
    const J: &str = "01a15961-0000-4000-8000-000000000002";

    #[test]
    fn unset_and_blank_are_no_bindings() {
        assert!(parse("X", None).unwrap().is_empty());
        assert!(parse("X", Some("  ")).unwrap().is_empty());
    }

    #[test]
    fn rows_map_external_ids_to_scopes() {
        let raw = format!(
            r#"[{{"external_id":" t1 ","tenant":"{T}","job_id":"{J}","job_key":"support"}}]"#
        );
        let b = parse("WEBCHAT_BINDINGS", Some(&raw)).unwrap();
        assert_eq!(b["t1"].job_key, "support");
        assert_eq!(b["t1"].tenant.to_string(), T);
    }

    #[test]
    fn malformed_blank_and_duplicate_fail_by_name_without_the_value() {
        let e = parse("TELEGRAM_BINDINGS", Some("secret-ish{"))
            .unwrap_err()
            .to_string();
        assert!(e.contains("TELEGRAM_BINDINGS") && !e.contains("secret-ish"));
        let blank =
            format!(r#"[{{"external_id":"","tenant":"{T}","job_id":"{J}","job_key":"k"}}]"#);
        assert!(parse("X", Some(&blank)).is_err());
        let row = format!(r#"{{"external_id":"a","tenant":"{T}","job_id":"{J}","job_key":"k"}}"#);
        assert!(parse("X", Some(&format!("[{row},{row}]"))).is_err());
        let extra =
            format!(r#"[{{"external_id":"a","tenant":"{T}","job_id":"{J}","job_key":"k","x":1}}]"#);
        assert!(parse("X", Some(&extra)).is_err());
    }
}
