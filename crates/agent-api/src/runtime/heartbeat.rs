//! Runtime process heartbeats (B4, new; read by the fleet view `busAgents` / `agentFleet`, T14).
//!
//! Every runtime process writes the root HASH `hb:{role}:{consumer}` every 10 s with a 60 s expiry: `role`,
//! `consumer`, `started_at` (ms), `beat_at` (ms), `version`, and `serving` — a JSON map scope →
//! `{release, rev, read_at}` of the design each scope task last read (T4 fills it through [`Serving`]). A process
//! that stops beating disappears within 60 s. At start it also ZADDs itself to the root ZSET `starts:{role}`
//! (score = start ms, 24 h window), which gives `restarts_24h`.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::Value;

use super::bus::{Bus, now_ms};
use super::scope::Scope;

/// How often a process beats.
pub const INTERVAL: Duration = Duration::from_secs(10);
/// How long a beat lives: six missed beats.
pub const TTL: Duration = Duration::from_secs(60);
/// The window of `starts:{role}`.
pub const STARTS_WINDOW: Duration = Duration::from_secs(24 * 3600);

/// The root key of one process's heartbeat.
pub fn key(root: &Bus, role: &str, consumer: &str) -> String {
    root.root_key(&format!("hb:{role}:{consumer}"))
}

/// The root key of a role's boots.
pub fn starts_key(root: &Bus, role: &str) -> String {
    root.root_key(&format!("starts:{role}"))
}

/// The design one scope task serves: the release (prod) or draft revision (test) and when it was read (ms).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ServingEntry {
    pub release: Option<String>,
    pub rev: Option<i64>,
    pub read_at: u64,
}

/// What this process serves, by scope text; shared between the scope tasks (writers) and the heartbeat (reader).
#[derive(Clone, Debug, Default)]
pub struct Serving(Arc<Mutex<BTreeMap<String, ServingEntry>>>);

impl Serving {
    /// Record that `scope` now serves `release` / `rev`, read now.
    pub fn set(&self, scope: &Scope, release: Option<String>, rev: Option<i64>) {
        let entry = ServingEntry {
            release,
            rev,
            read_at: now_ms(),
        };
        self.lock().insert(scope.to_string(), entry);
    }

    /// Forget `scope` (its task stopped).
    pub fn remove(&self, scope: &Scope) {
        self.lock().remove(&scope.to_string());
    }

    /// The map as the `serving` JSON field.
    pub fn snapshot(&self) -> Value {
        serde_json::to_value(&*self.lock()).unwrap_or(Value::Null)
    }

    /// A poisoned lock still holds a usable map (a writer panicked between two plain inserts).
    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, ServingEntry>> {
        self.0.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// Timing of [`run_with`]; [`run`] uses [`INTERVAL`] / [`TTL`].
#[derive(Clone, Copy, Debug)]
pub struct Timing {
    pub interval: Duration,
    pub ttl: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            interval: INTERVAL,
            ttl: TTL,
        }
    }
}

/// Record this process's start in `starts:{role}` and drop starts older than 24 h.
pub async fn record_start(root: &Bus, role: &str, consumer: &str, started_at: u64) -> Result<()> {
    let key = starts_key(root, role);
    let window = u64::try_from(STARTS_WINDOW.as_millis()).unwrap_or(u64::MAX);
    let cutoff = started_at.saturating_sub(window);
    let mut pipe = redis::pipe();
    pipe.cmd("ZADD")
        .arg(&key)
        .arg(started_at)
        .arg(format!("{consumer}@{started_at}"))
        .ignore()
        .cmd("ZREMRANGEBYSCORE")
        .arg(&key)
        .arg("-inf")
        .arg(format!("({cutoff}"))
        .ignore()
        .cmd("EXPIRE")
        .arg(&key)
        .arg(STARTS_WINDOW.as_secs())
        .ignore();
    let mut c = root.conn();
    pipe.query_async::<()>(&mut c)
        .await
        .with_context(|| format!("ZADD {key}"))
}

/// One beat: the HASH with every field, then its expiry, in one MULTI.
pub async fn beat(
    root: &Bus,
    role: &str,
    consumer: &str,
    started_at: u64,
    serving: &Serving,
    ttl: Duration,
) -> Result<()> {
    let key = key(root, role, consumer);
    let mut pipe = redis::pipe();
    pipe.atomic();
    pipe.cmd("HSET")
        .arg(&key)
        .arg("role")
        .arg(role)
        .arg("consumer")
        .arg(consumer)
        .arg("started_at")
        .arg(started_at)
        .arg("beat_at")
        .arg(now_ms())
        .arg("version")
        .arg(env!("CARGO_PKG_VERSION"))
        .arg("serving")
        .arg(serving.snapshot().to_string())
        .ignore()
        .cmd("PEXPIRE")
        .arg(&key)
        .arg(u64::try_from(ttl.as_millis()).unwrap_or(u64::MAX))
        .ignore();
    let mut c = root.conn();
    pipe.query_async::<()>(&mut c)
        .await
        .with_context(|| format!("HSET {key}"))
}

/// Beat every 10 s with a 60 s expiry until the task is aborted. Never returns; a failed beat is logged and the
/// next one retries.
pub async fn run(root: Bus, role: String, consumer: String, serving: Serving) {
    run_with(root, role, consumer, serving, Timing::default()).await
}

/// [`run`] with explicit timing (tests).
pub async fn run_with(root: Bus, role: String, consumer: String, serving: Serving, timing: Timing) {
    let started_at = now_ms();
    if let Err(e) = record_start(&root, &role, &consumer, started_at).await {
        tracing::warn!(role = %role, error = %e, "recording the start failed");
    }
    let mut tick = tokio::time::interval(timing.interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        if let Err(e) = beat(&root, &role, &consumer, started_at, &serving, timing.ttl).await {
            tracing::warn!(role = %role, error = %e, "heartbeat failed; retrying");
        }
    }
}
