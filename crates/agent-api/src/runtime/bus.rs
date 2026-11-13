//! Redis Streams bus: XADD to publish, consumer groups to share work, XACK on success, XAUTOCLAIM to take over
//! entries a dead consumer left pending, and a dead-letter stream after `max_deliveries`. Ported from
//! care-bus-agents@96c608c `crates/carebus/src/bus.rs` (B4 T1).
//!
//! Delivery is at-least-once. Every handler must be idempotent.
//!
//! B4 changes against the port: the scope carries the environment (R5) and every envelope is stamped with it, so a
//! test-env envelope on a prod stream (or the reverse) is dead-lettered as `scope_mismatch`; the worker and tool
//! streams are replaced by the operation streams `bus:op` (`op.call`, read by the runner) and `bus:op:results`
//! (`op.result`, read by the orchestrator, R1); the console mirror redacts `op.*` payloads to ids and codes (K50)
//! and keeps the last 500 frames in `console:recent` (1 day) for the JSON backlog and `Last-Event-ID` replay.

use std::future::Future;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use redis::aio::{ConnectionManager, ConnectionManagerConfig, MultiplexedConnection};
use redis::streams::{
    StreamAutoClaimReply, StreamId, StreamInfoGroupsReply, StreamPendingCountReply, StreamReadReply,
};
use redis::{AsyncCommands, AsyncConnectionConfig, Client};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use uuid::Uuid;

use super::scope::{Env, Scope};

/// The events on the bus. A topic is `[<context-space>.]<event>`; [`event_of`] strips the prefix.
pub mod topics {
    /// interaction (drain) → orchestrator on `bus:customer:{area}`.
    pub const REQUEST_QUALIFIED: &str = "request.qualified";
    /// orchestrator → interaction on `bus:customer:{area}`.
    pub const REQUEST_RESOLVED: &str = "request.resolved";
    /// orchestrator → runner on `bus:op`.
    pub const OP_CALL: &str = "op.call";
    /// runner → orchestrator on `bus:op:results`.
    pub const OP_RESULT: &str = "op.result";
    /// Every entry on `bus:dead`.
    pub const DEAD_LETTER: &str = "dead.letter";
    /// The events [`super::event_of`] recognises (dead letters have no recipient group).
    pub const EVENTS: [&str; 4] = [REQUEST_QUALIFIED, REQUEST_RESOLVED, OP_CALL, OP_RESULT];
}

/// The consumer groups (one per runtime role that reads a stream).
pub mod groups {
    pub const ORCHESTRATOR: &str = "orchestrator";
    pub const INTERACTION: &str = "interaction";
    pub const RUNNER: &str = "runner";
}

/// The event `topic` carries, ignoring the context-space prefix: `returns.request.qualified` and
/// `request.qualified` both give `request.qualified`; the suffix must start at a dot.
pub fn event_of(topic: &str) -> Option<&'static str> {
    topics::EVENTS
        .into_iter()
        .find(|e| topic == *e || topic.strip_suffix(e).is_some_and(|p| p.ends_with('.')))
}

/// The one message shape on every stream. Stored as a single JSON field.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Envelope {
    pub id: String,
    pub topic: String,
    pub from: String,
    pub correlation_id: String,
    pub published_at_ms: u64,
    pub payload: Value,
    /// `name@version` of the procedure version running the case, on the orchestrator's envelopes. Absent on every
    /// other envelope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub procedure: Option<String>,
    /// The scope the envelope was published in, stamped by `Bus::publish` (only a scoped bus publishes, so on the
    /// wire all three are always present). `Option` because `Envelope::new` is scope-agnostic.
    #[serde(default)]
    pub tenant_id: Option<Uuid>,
    #[serde(default)]
    pub job: Option<String>,
    #[serde(default)]
    pub env: Option<Env>,
}

impl Envelope {
    pub fn new(
        topic: &str,
        from: &str,
        correlation_id: &str,
        payload: impl Serialize,
    ) -> Result<Self> {
        Ok(Self {
            id: Uuid::new_v4().to_string(),
            topic: topic.into(),
            from: from.into(),
            correlation_id: correlation_id.into(),
            published_at_ms: now_ms(),
            payload: serde_json::to_value(payload)?,
            procedure: None,
            tenant_id: None,
            job: None,
            env: None,
        })
    }

    /// Stamp the procedure version (`name@version`) the case runs.
    pub fn with_procedure(mut self, procedure: Option<&str>) -> Self {
        self.procedure = procedure.map(String::from);
        self
    }

    pub fn payload_as<T: DeserializeOwned>(&self) -> Result<T> {
        serde_json::from_value(self.payload.clone())
            .with_context(|| format!("payload of {} is not the expected shape", self.topic))
    }

    /// The event this envelope carries, ignoring the context-space prefix.
    pub fn event(&self) -> Option<&'static str> {
        event_of(&self.topic)
    }

    /// The `<tenant>/<job>/<env>` stamped on it, or `no scope` when any part is missing.
    fn stamped(&self) -> String {
        match (&self.tenant_id, &self.job, &self.env) {
            (Some(t), Some(j), Some(e)) => format!("{t}/{j}/{e}"),
            _ => "no scope".to_string(),
        }
    }
}

/// Milliseconds since the epoch (0 before 1970, which no clock here reports).
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// The top-level payload fields of an `op.*` envelope that reach the console (K50: ids and outcome codes). Only
/// scalar values are kept; anything else — inputs, outputs, customer data, nested objects — is dropped.
pub const CONSOLE_OP_FIELDS: &[&str] = &[
    "call_id",
    "case_id",
    "operation",
    "phase",
    "step",
    "attempt",
    "route_kind",
    "state",
    "outcome_code",
    "error_code",
    "latency_ms",
];

/// What the console (live mirror and `console:recent`) shows of `env`: the envelope itself, except that an `op.*`
/// payload is reduced to [`CONSOLE_OP_FIELDS`], also inside a dead letter's `original`.
pub fn console_view(env: &Envelope) -> Value {
    let mut v = serde_json::to_value(env).unwrap_or(Value::Null);
    if is_op(&env.topic) {
        v["payload"] = redact_op_payload(&env.payload);
    } else if env.topic == topics::DEAD_LETTER
        && let Some(orig) = v.get_mut("payload").and_then(|p| p.get_mut("original"))
        && orig.get("topic").and_then(Value::as_str).is_some_and(is_op)
    {
        let redacted = redact_op_payload(orig.get("payload").unwrap_or(&Value::Null));
        orig["payload"] = redacted;
    }
    v
}

fn is_op(topic: &str) -> bool {
    matches!(event_of(topic), Some(topics::OP_CALL | topics::OP_RESULT))
}

fn redact_op_payload(payload: &Value) -> Value {
    let mut out = Map::new();
    if let Some(obj) = payload.as_object() {
        for k in CONSOLE_OP_FIELDS {
            if let Some(v) = obj.get(*k)
                && !v.is_object()
                && !v.is_array()
            {
                out.insert((*k).to_string(), v.clone());
            }
        }
    }
    Value::Object(out)
}

/// Stream and key names. The prefix isolates environments, test runs and scopes.
#[derive(Clone, Debug)]
pub struct Streams {
    prefix: String,
    /// The scope and root prefix of a scoped bus: [`Streams::key`] goes through [`Scope::key`] (the prod/test guard).
    scope: Option<(Scope, String)>,
}

impl Streams {
    /// Unscoped names under `prefix` (tests and the root bus).
    pub fn new(prefix: impl Into<String>) -> Self {
        Self {
            prefix: prefix.into(),
            scope: None,
        }
    }

    /// The names of `scope` under the environment prefix `root` (R5).
    pub fn scoped(root: &str, scope: &Scope) -> Self {
        Self {
            prefix: scope.prefix(root),
            scope: Some((scope.clone(), root.to_string())),
        }
    }

    /// interaction ↔ orchestrator for one department: `request.qualified` down, `request.resolved` back.
    pub fn customer(&self, area: &str) -> String {
        self.key(&format!("bus:customer:{area}"))
    }
    /// orchestrator → runner: `op.call`.
    pub fn op(&self) -> String {
        self.key("bus:op")
    }
    /// runner → orchestrator: `op.result`.
    pub fn op_results(&self) -> String {
        self.key("bus:op:results")
    }
    pub fn dead(&self) -> String {
        self.key("bus:dead")
    }
    /// Pub/Sub channel mirroring every publish and the recipients' acks, for the live console (lossy).
    pub fn console(&self) -> String {
        self.key("console")
    }
    /// LIST of the last [`CONSOLE_RECENT_CAP`] console frames, newest first (1 day): the JSON backlog.
    pub fn console_recent(&self) -> String {
        self.key("console:recent")
    }
    /// Pub/Sub channel of the chat frames (turn machinery, T8/T9).
    pub fn chat(&self) -> String {
        self.key("chat")
    }
    pub fn key(&self, k: &str) -> String {
        match &self.scope {
            Some((scope, root)) => scope.key(root, k),
            None => format!("{}{k}", self.prefix),
        }
    }
    /// The prefix every key above starts with (environment prefix, plus the scope on a scoped bus).
    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    /// The consumer group an entry is FOR: the one whose handler acts on `topic` on `stream` (prefixed or not).
    /// Two groups read `bus:customer:{area}` (orchestrator and interaction) and both XACK every entry, so only the
    /// recipient's ack says the entry was handled. `None` for the dead-letter stream and unknown topics. This is
    /// the only copy of the stream → group topology outside the agents' own `consume` calls.
    pub fn consumer_group(&self, stream: &str, topic: &str) -> Option<String> {
        let key = stream.strip_prefix(self.prefix.as_str()).unwrap_or(stream);
        let parts: Vec<&str> = key.split(':').collect();
        let event = event_of(topic)?;
        let group = match (parts.as_slice(), event) {
            (["bus", "customer", _], topics::REQUEST_QUALIFIED) => groups::ORCHESTRATOR,
            (["bus", "customer", _], topics::REQUEST_RESOLVED) => groups::INTERACTION,
            (["bus", "op"], topics::OP_CALL) => groups::RUNNER,
            (["bus", "op", "results"], topics::OP_RESULT) => groups::ORCHESTRATOR,
            _ => return None,
        };
        Some(group.to_string())
    }
}

/// The `scope_mismatch` dead-letter reason of an envelope stamped for another scope than the stream's, else `None`.
fn scope_mismatch(scope: &Scope, env: &Envelope) -> Option<String> {
    (env.tenant_id != Some(scope.tenant)
        || env.job.as_deref() != Some(scope.job.as_str())
        || env.env != Some(scope.env))
    .then(|| {
        format!(
            "scope_mismatch: stamped {}, stream is {scope}",
            env.stamped()
        )
    })
}

/// How many console frames `console:recent` keeps.
pub const CONSOLE_RECENT_CAP: isize = 500;
/// How long `console:recent` and the delivery counters live without a write.
pub const DAY_SECS: i64 = 86_400;
/// How long a `metrics:{group}:{minute}` hash lives (the fleet view reads the last hour).
pub const METRICS_TTL_SECS: i64 = 7_200;

/// `a <= b` for stream entry ids `<ms>-<seq>`; an unparseable id compares as not-before.
fn id_le(a: &str, b: &str) -> bool {
    let p = |s: &str| -> Option<(u64, u64)> {
        let (ms, seq) = s.split_once('-')?;
        Some((ms.parse().ok()?, seq.parse().ok()?))
    };
    matches!((p(a), p(b)), (Some(a), Some(b)) if a <= b)
}

#[derive(Clone, Debug)]
pub struct ConsumeOptions {
    pub batch: usize,
    pub block: Duration,
    /// An entry pending longer than this is taken over from its consumer.
    pub reclaim_idle: Duration,
    pub max_deliveries: u64,
}

/// The default [`ConsumeOptions::reclaim_idle`]: right for a handler that finishes in seconds.
pub const DEFAULT_RECLAIM_IDLE: Duration = Duration::from_secs(30);
/// Slack a consumer's `reclaim_idle` keeps above its longest handler (Redis, Postgres, scheduling).
pub const RECLAIM_SLACK: Duration = Duration::from_secs(10);

impl Default for ConsumeOptions {
    fn default() -> Self {
        Self {
            batch: 16,
            block: Duration::from_secs(2),
            reclaim_idle: DEFAULT_RECLAIM_IDLE,
            max_deliveries: 5,
        }
    }
}

/// `reclaim_idle` for a consumer whose longest handler runs `handler_budget`: that plus [`RECLAIM_SLACK`], never
/// below [`DEFAULT_RECLAIM_IDLE`]. An entry idle longer than this is taken over by ANY consumer of the group (a peer
/// replica, or the same one), so a healthy slow handler must finish inside it or its work runs twice.
pub const fn reclaim_idle_for(handler_budget: Duration) -> Duration {
    let secs = handler_budget.as_secs() + RECLAIM_SLACK.as_secs();
    if secs > DEFAULT_RECLAIM_IDLE.as_secs() {
        Duration::from_secs(secs)
    } else {
        DEFAULT_RECLAIM_IDLE
    }
}

impl ConsumeOptions {
    /// The defaults with [`reclaim_idle_for`] `handler_budget`, and `batch` 1: a read batch is handled one entry
    /// after another, and every entry of it is pending (its idle clock running) from the read, so entry N would
    /// wait N-1 handlers and a peer could take it over before this consumer reached it.
    pub fn for_handler_budget(handler_budget: Duration) -> Self {
        Self {
            batch: 1,
            reclaim_idle: reclaim_idle_for(handler_budget),
            ..Self::default()
        }
    }
}

#[derive(Clone)]
pub struct Bus {
    client: Client,
    conn: ConnectionManager,
    /// Key names. On a scoped bus the prefix is the scope's (R5).
    pub streams: Streams,
    maxlen: usize,
    /// The environment prefix (`AGENT_PREFIX`), kept apart from the scope.
    root_prefix: String,
    /// `None` on the root bus, which may only touch root keys (registry, nudges, heartbeats).
    scope: Option<Scope>,
}

/// `Debug` never prints the connection (its URL may hold a password).
impl std::fmt::Debug for Bus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bus")
            .field("root_prefix", &self.root_prefix)
            .field("scope", &self.scope)
            .finish_non_exhaustive()
    }
}

impl Bus {
    /// Connect now (the runtime processes: no Redis, no work). The error never contains the URL.
    pub async fn connect(url: &str, prefix: &str) -> Result<Self> {
        Self::connect_with_config(url, prefix, ConnectionManagerConfig::new()).await
    }

    /// [`Bus::connect`] with a per-command response timeout other than the redis crate's default (500 ms).
    pub async fn connect_with_response_timeout(
        url: &str,
        prefix: &str,
        response_timeout: Duration,
    ) -> Result<Self> {
        let cfg = ConnectionManagerConfig::new().set_response_timeout(Some(response_timeout));
        Self::connect_with_config(url, prefix, cfg).await
    }

    async fn connect_with_config(
        url: &str,
        prefix: &str,
        cfg: ConnectionManagerConfig,
    ) -> Result<Self> {
        let client = Client::open(url).context("REDIS_URL is not a valid redis URL")?;
        let conn = ConnectionManager::new_with_config(client.clone(), cfg)
            .await
            .context("connect to Redis (REDIS_URL)")?;
        Ok(Self::with(client, conn, prefix))
    }

    /// Connect on first use (agent-api: a Redis outage must not stop the admin API from booting). Commands fail
    /// fast while Redis is down: 2 s per connection attempt, 2 retries.
    pub fn connect_lazy(url: &str, prefix: &str) -> Result<Self> {
        let client = Client::open(url).context("REDIS_URL is not a valid redis URL")?;
        let cfg = ConnectionManagerConfig::new()
            .set_connection_timeout(Some(Duration::from_secs(2)))
            .set_number_of_retries(2);
        let conn = ConnectionManager::new_lazy_with_config(client.clone(), cfg)
            .context("set up the Redis connection (REDIS_URL)")?;
        Ok(Self::with(client, conn, prefix))
    }

    fn with(client: Client, conn: ConnectionManager, prefix: &str) -> Self {
        Self {
            client,
            conn,
            streams: Streams::new(prefix),
            maxlen: 100_000,
            root_prefix: prefix.to_string(),
            scope: None,
        }
    }

    /// The same client and connection, with every key under the scope's prefix.
    pub fn scoped(&self, s: &Scope) -> Bus {
        Bus {
            client: self.client.clone(),
            conn: self.conn.clone(),
            streams: Streams::scoped(&self.root_prefix, s),
            maxlen: self.maxlen,
            root_prefix: self.root_prefix.clone(),
            scope: Some(s.clone()),
        }
    }

    /// The root bus of this one (the same connection, no scope).
    pub fn root(&self) -> Bus {
        Bus {
            client: self.client.clone(),
            conn: self.conn.clone(),
            streams: Streams::new(self.root_prefix.clone()),
            maxlen: self.maxlen,
            root_prefix: self.root_prefix.clone(),
            scope: None,
        }
    }

    pub fn scope(&self) -> Option<&Scope> {
        self.scope.as_ref()
    }

    /// The environment prefix (`AGENT_PREFIX`).
    pub fn root_prefix(&self) -> &str {
        &self.root_prefix
    }

    /// `{root_prefix}{k}`, unscoped: registry, nudges, heartbeats.
    pub fn root_key(&self, k: &str) -> String {
        format!("{}{k}", self.root_prefix)
    }

    /// The scope, or `bus is not scoped` naming what was refused (in the message itself, not a context layer, so
    /// the plain `to_string()` of the error says it).
    fn require_scope(&self, what: &str) -> Result<&Scope> {
        self.scope
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("bus is not scoped: refusing {what}"))
    }

    /// The underlying client, for dedicated connections (Pub/Sub subscriptions).
    pub fn client(&self) -> &Client {
        &self.client
    }

    /// Shared connection for non-blocking commands.
    pub fn conn(&self) -> ConnectionManager {
        self.conn.clone()
    }

    /// XADD `env` stamped with this bus's scope, then mirror it on the console. A root bus refuses.
    pub async fn publish(&self, stream: &str, env: &Envelope) -> Result<String> {
        let scope = self.require_scope(&format!("XADD {stream}"))?;
        let mut env = env.clone();
        env.tenant_id = Some(scope.tenant);
        env.job = Some(scope.job.clone());
        env.env = Some(scope.env);
        let json = serde_json::to_string(&env)?;
        let mut c = self.conn.clone();
        let id: String = redis::cmd("XADD")
            .arg(stream)
            .arg("MAXLEN")
            .arg("~")
            .arg(self.maxlen)
            .arg("*")
            .arg("envelope")
            .arg(&json)
            .query_async(&mut c)
            .await
            .with_context(|| format!("XADD {stream}"))?;
        // `acked` is false at XADD time; the recipient's XACK follows as a `kind: "ack"` frame (see `mirror_ack`).
        let group = self.streams.consumer_group(stream, &env.topic);
        let acked = group.as_ref().map(|_| false);
        let frame = json!({
            "stream": stream,
            "entry_id": id,
            "envelope": console_view(&env),
            "consumer_group": group,
            "acked": acked,
        });
        // The console mirror is lossy by design: the entry is already on the stream, so a failed mirror must not
        // make the caller think nothing was published.
        if let Err(e) = self.mirror(&frame).await {
            tracing::warn!(stream, entry_id = %id, error = %e, "console mirror failed; entry is on the stream");
        }
        Ok(id)
    }

    /// PUBLISH `frame` on the console channel and keep it in `console:recent` (capped, 1 day).
    async fn mirror(&self, frame: &Value) -> redis::RedisResult<()> {
        let text = frame.to_string();
        let recent = self.streams.console_recent();
        let mut pipe = redis::pipe();
        pipe.cmd("PUBLISH")
            .arg(self.streams.console())
            .arg(&text)
            .ignore()
            .cmd("LPUSH")
            .arg(&recent)
            .arg(&text)
            .ignore()
            .cmd("LTRIM")
            .arg(&recent)
            .arg(0)
            .arg(CONSOLE_RECENT_CAP - 1)
            .ignore()
            .cmd("EXPIRE")
            .arg(&recent)
            .arg(DAY_SECS)
            .ignore();
        let mut c = self.conn.clone();
        pipe.query_async::<()>(&mut c).await
    }

    /// Whether `group` has acknowledged `entry_id` on `stream` (full key), computed from Redis itself, so a reader
    /// that missed the live `ack` frame gets the truth: the group has delivered the entry (its last-delivered-id is
    /// at or past it) and the entry is no longer in the group's pending list. A missing stream or group means
    /// nobody has read it: not acked.
    pub async fn is_acked(&self, stream: &str, group: &str, entry_id: &str) -> Result<bool> {
        let mut c = self.conn.clone();
        let groups: redis::RedisResult<StreamInfoGroupsReply> = redis::cmd("XINFO")
            .arg("GROUPS")
            .arg(stream)
            .query_async(&mut c)
            .await;
        let groups = match groups {
            Ok(g) => g,
            Err(e) if e.to_string().contains("no such key") => return Ok(false),
            Err(e) => return Err(e).with_context(|| format!("XINFO GROUPS {stream}")),
        };
        let Some(g) = groups.groups.iter().find(|g| g.name == group) else {
            return Ok(false);
        };
        if !id_le(entry_id, &g.last_delivered_id) {
            return Ok(false);
        }
        let pending: StreamPendingCountReply = redis::cmd("XPENDING")
            .arg(stream)
            .arg(group)
            .arg(entry_id)
            .arg(entry_id)
            .arg(1)
            .query_async(&mut c)
            .await
            .with_context(|| format!("XPENDING {stream} {group} {entry_id}"))?;
        Ok(pending.ids.is_empty())
    }

    /// Mirror the recipient group's XACK on the console, lossy like the publish mirror. Acks by the other groups
    /// that read the same stream (and skip the entry) are not mirrored: they say nothing about handling.
    async fn mirror_ack(&self, stream: &str, group: &str, entry_id: &str, env: &Envelope) {
        if self.streams.consumer_group(stream, &env.topic).as_deref() != Some(group) {
            return;
        }
        let frame = json!({
            "kind": "ack",
            "stream": stream,
            "entry_id": entry_id,
            "consumer_group": group,
            "correlation_id": env.correlation_id,
        });
        if let Err(e) = self.mirror(&frame).await {
            tracing::warn!(stream, group, entry_id, error = %e, "console ack mirror failed; the XACK stands");
        }
    }

    pub async fn ensure_group(&self, stream: &str, group: &str) -> Result<()> {
        self.require_scope(&format!("XGROUP CREATE {stream} {group}"))?;
        let mut c = self.conn.clone();
        let r: redis::RedisResult<()> = redis::cmd("XGROUP")
            .arg("CREATE")
            .arg(stream)
            .arg(group)
            .arg("0")
            .arg("MKSTREAM")
            .query_async(&mut c)
            .await;
        match r {
            Ok(()) => Ok(()),
            Err(e) if e.code() == Some("BUSYGROUP") => Ok(()),
            Err(e) => Err(e).with_context(|| format!("XGROUP CREATE {stream} {group}")),
        }
    }

    /// Consume `stream` as `consumer` in `group` until the task is dropped. `Ok` acks the entry; `Err` leaves it
    /// pending for redelivery.
    pub async fn consume<F, Fut>(
        &self,
        stream: &str,
        group: &str,
        consumer: &str,
        opts: ConsumeOptions,
        handler: F,
    ) -> Result<()>
    where
        F: Fn(Envelope) -> Fut,
        Fut: Future<Output = Result<()>>,
    {
        self.ensure_group(stream, group).await?;
        // A blocking read needs its own connection: on a shared multiplexed connection BLOCK stalls every other
        // command, and the default response timeout would cut the block short.
        let cfg = AsyncConnectionConfig::new().set_response_timeout(None);
        let mut reader: MultiplexedConnection = self
            .client
            .get_multiplexed_async_connection_with_config(&cfg)
            .await?;
        loop {
            let claimed: StreamAutoClaimReply = redis::cmd("XAUTOCLAIM")
                .arg(stream)
                .arg(group)
                .arg(consumer)
                .arg(u64::try_from(opts.reclaim_idle.as_millis()).unwrap_or(u64::MAX))
                .arg("0-0")
                .arg("COUNT")
                .arg(opts.batch)
                .query_async(&mut reader)
                .await?;
            for entry in &claimed.claimed {
                self.process(stream, group, entry, &opts, &handler).await?;
            }
            let read: Option<StreamReadReply> = redis::cmd("XREADGROUP")
                .arg("GROUP")
                .arg(group)
                .arg(consumer)
                .arg("COUNT")
                .arg(opts.batch)
                .arg("BLOCK")
                .arg(u64::try_from(opts.block.as_millis()).unwrap_or(u64::MAX))
                .arg("STREAMS")
                .arg(stream)
                .arg(">")
                .query_async(&mut reader)
                .await?;
            for key in read.map(|r| r.keys).unwrap_or_default() {
                for entry in &key.ids {
                    self.process(stream, group, entry, &opts, &handler).await?;
                }
            }
        }
    }

    /// `consume` on several streams at once with the same group and handler. Returns when any of them fails.
    pub async fn consume_all<F, Fut>(
        &self,
        streams: Vec<String>,
        group: &str,
        consumer: &str,
        opts: ConsumeOptions,
        handler: F,
    ) -> Result<()>
    where
        F: Fn(Envelope) -> Fut + Clone,
        Fut: Future<Output = Result<()>>,
    {
        self.require_scope(&format!("consume_all as {group}"))?;
        let runs = streams
            .iter()
            .map(|s| self.consume(s, group, consumer, opts.clone(), handler.clone()));
        futures_util::future::try_join_all(runs).await?;
        Ok(())
    }

    /// The delivery counter of one entry for one group (`deliveries:{stream}:{group}:{id}`, 1 day).
    fn deliveries_key(&self, stream: &str, group: &str, entry_id: &str) -> String {
        self.streams
            .key(&format!("deliveries:{stream}:{group}:{entry_id}"))
    }

    async fn process<F, Fut>(
        &self,
        stream: &str,
        group: &str,
        entry: &StreamId,
        opts: &ConsumeOptions,
        handler: &F,
    ) -> Result<()>
    where
        F: Fn(Envelope) -> Fut,
        Fut: Future<Output = Result<()>>,
    {
        let scope = self.require_scope(&format!("consume {stream}"))?;
        let mut c = self.conn.clone();
        let counter = self.deliveries_key(stream, group, &entry.id);
        let deliveries: u64 = c.incr(&counter, 1).await?;
        let _: bool = c.expire(&counter, DAY_SECS).await?;

        let env = entry
            .get::<String>("envelope")
            .and_then(|raw| serde_json::from_str::<Envelope>(&raw).ok());
        let Some(env) = env else {
            return self
                .dead_letter(
                    stream,
                    group,
                    &entry.id,
                    deliveries,
                    "unparseable envelope",
                    None,
                )
                .await;
        };
        if let Some(reason) = scope_mismatch(scope, &env) {
            return self
                .dead_letter(stream, group, &entry.id, deliveries, &reason, Some(&env))
                .await;
        }
        if deliveries > opts.max_deliveries {
            let reason = format!("gave up after {} deliveries", deliveries - 1);
            return self
                .dead_letter(stream, group, &entry.id, deliveries, &reason, Some(&env))
                .await;
        }
        // only the recipient group's handling counts (both customer-stream groups ack every entry)
        let recipient = self.streams.consumer_group(stream, &env.topic).as_deref() == Some(group);
        match handler(env.clone()).await {
            Ok(()) => {
                self.ack(stream, group, &entry.id, Some(&env)).await?;
                if recipient {
                    self.count(group, "handled").await;
                }
            }
            Err(e) => {
                tracing::warn!(stream, group, entry = %entry.id, topic = %env.topic, deliveries, "handler failed, left pending: {e:#}");
                if recipient {
                    self.count(group, "errors").await;
                }
            }
        }
        Ok(())
    }

    /// Dead-letter one pending `entry` of `group` on `stream` (a full key) that an operator took over
    /// (`claimStuck`, bus_admin): `reason` for a well-formed envelope of this scope, else the same reason a consumer
    /// would give (`unparseable envelope`, `scope_mismatch: …`, so a replay can never move it into this scope).
    /// The delivery count is the entry's counter so far (it is not a delivery of its own).
    pub async fn dead_letter_taken_over(
        &self,
        stream: &str,
        group: &str,
        entry: &StreamId,
        reason: &str,
    ) -> Result<()> {
        let scope = self.require_scope(&format!("dead-letter on {stream}"))?;
        let mut c = self.conn.clone();
        let deliveries: Option<u64> = c.get(self.deliveries_key(stream, group, &entry.id)).await?;
        let deliveries = deliveries.unwrap_or(0);
        let env = entry
            .get::<String>("envelope")
            .and_then(|raw| serde_json::from_str::<Envelope>(&raw).ok());
        let Some(env) = env else {
            return self
                .dead_letter(
                    stream,
                    group,
                    &entry.id,
                    deliveries,
                    "unparseable envelope",
                    None,
                )
                .await;
        };
        let reason = scope_mismatch(scope, &env).unwrap_or_else(|| reason.to_string());
        self.dead_letter(stream, group, &entry.id, deliveries, &reason, Some(&env))
            .await
    }

    /// `metrics:{group}:{minute}` (scoped HASH, [`METRICS_TTL_SECS`]): the group's `handled` / `errors` in that
    /// minute (`minute` = epoch ms / 60 000), read by the fleet view (`bus_admin`, T14).
    pub fn metrics_key(&self, group: &str, minute: u64) -> String {
        self.streams.key(&format!("metrics:{group}:{minute}"))
    }

    /// HINCRBY this minute's `field` of `group`'s metrics hash. Lossy: a failure is logged, never fails the handling.
    async fn count(&self, group: &str, field: &str) {
        let key = self.metrics_key(group, now_ms() / 60_000);
        let mut pipe = redis::pipe();
        pipe.cmd("HINCRBY")
            .arg(&key)
            .arg(field)
            .arg(1)
            .ignore()
            .cmd("EXPIRE")
            .arg(&key)
            .arg(METRICS_TTL_SECS)
            .ignore();
        let mut c = self.conn.clone();
        if let Err(e) = pipe.query_async::<()>(&mut c).await {
            tracing::warn!(group, field, error = %e, "metrics counter failed; the handling stands");
        }
    }

    /// XACK `entry_id` for `group`, drop its delivery counter and mirror the recipient's ack on the console.
    pub async fn ack(
        &self,
        stream: &str,
        group: &str,
        entry_id: &str,
        env: Option<&Envelope>,
    ) -> Result<()> {
        let mut c = self.conn.clone();
        let _: i64 = c
            .xack(stream, group, &[entry_id])
            .await
            .with_context(|| format!("XACK {stream} {group} {entry_id}"))?;
        let _: i64 = c.del(self.deliveries_key(stream, group, entry_id)).await?;
        if let Some(env) = env {
            self.mirror_ack(stream, group, entry_id, env).await;
        }
        Ok(())
    }

    /// Publish a `dead.letter` for `entry_id` on `bus:dead` (with the reason and the original envelope, if it
    /// parsed), then ack the entry so it is never redelivered.
    pub async fn dead_letter(
        &self,
        stream: &str,
        group: &str,
        entry_id: &str,
        deliveries: u64,
        reason: &str,
        env: Option<&Envelope>,
    ) -> Result<()> {
        let cid = env.map(|e| e.correlation_id.as_str()).unwrap_or("unknown");
        let dead = Envelope::new(
            topics::DEAD_LETTER,
            group,
            cid,
            json!({ "stream": stream, "entry_id": entry_id, "deliveries": deliveries, "reason": reason, "original": env }),
        )?;
        self.publish(&self.streams.dead(), &dead).await?;
        self.ack(stream, group, entry_id, env).await?;
        tracing::error!(stream, group, entry = %entry_id, reason, "dead-lettered");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{Envelope, Streams, console_view, event_of, topics};
    use serde_json::json;

    #[test]
    fn stream_keys_follow_the_contract() {
        let s = Streams::new("p:");
        assert_eq!(s.customer("payments"), "p:bus:customer:payments");
        assert_eq!(s.op(), "p:bus:op");
        assert_eq!(s.op_results(), "p:bus:op:results");
        assert_eq!(s.dead(), "p:bus:dead");
        assert_eq!(s.console(), "p:console");
        assert_eq!(s.console_recent(), "p:console:recent");
    }

    #[test]
    fn consumer_group_is_the_recipient_of_the_topic_on_that_stream() {
        let s = Streams::new("p:");
        let g = |stream: &str, topic: &str| s.consumer_group(stream, topic);
        let some = |v: &str| Some(v.to_string());
        for pre in ["p:", ""] {
            let k = |k: &str| format!("{pre}{k}");
            assert_eq!(
                g(&k("bus:customer:returns"), "returns.request.qualified"),
                some("orchestrator")
            );
            assert_eq!(
                g(&k("bus:customer:faq"), "faq.request.resolved"),
                some("interaction")
            );
            assert_eq!(g(&k("bus:op"), "op.call"), some("runner"));
            assert_eq!(
                g(&k("bus:op:results"), "returns.op.result"),
                some("orchestrator")
            );
        }
        assert_eq!(g("p:bus:op", "op.result"), None);
        assert_eq!(g("p:bus:dead", "dead.letter"), None);
        assert_eq!(g("p:bus:customer:faq", "faq.marker"), None);
    }

    #[test]
    fn event_of_accepts_prefixed_and_bare_topics() {
        assert_eq!(
            event_of("returns.request.qualified"),
            Some(topics::REQUEST_QUALIFIED)
        );
        assert_eq!(event_of("op.call"), Some(topics::OP_CALL));
        assert_eq!(
            event_of("returns.xop.call"),
            None,
            "suffix must start at a dot"
        );
        assert_eq!(event_of("dead.letter"), None);
    }

    #[test]
    fn entry_ids_compare_numerically_not_as_strings() {
        assert!(super::id_le("9-0", "10-0"));
        assert!(super::id_le("10-2", "10-2"));
        assert!(!super::id_le("10-10", "10-9"));
        assert!(!super::id_le("1-0", "0-0"));
        assert!(!super::id_le("x", "10-0"));
    }

    #[test]
    fn console_view_keeps_ids_and_codes_of_op_payloads_only() {
        let call = Envelope::new(
            "op.call",
            "orchestrator",
            "CS-1",
            json!({"call_id": "c1", "case_id": "CS-1", "operation": "order.read",
                   "inputs": {"email": "ada@example.test"}, "note": "customer text", "attempt": 1,
                   "state": {"nested": true}}),
        )
        .unwrap();
        let v = console_view(&call);
        assert_eq!(
            v["payload"],
            json!({"call_id": "c1", "case_id": "CS-1", "operation": "order.read", "attempt": 1})
        );
        assert_eq!(v["correlation_id"], "CS-1");
        let dead = Envelope::new(
            "dead.letter",
            "runner",
            "CS-1",
            json!({"reason": "x", "original": serde_json::to_value(call).unwrap()}),
        )
        .unwrap();
        let v = console_view(&dead);
        assert_eq!(
            v["payload"]["original"]["payload"],
            json!({"call_id": "c1", "case_id": "CS-1", "operation": "order.read", "attempt": 1})
        );
        assert!(!v.to_string().contains("ada@example.test"));
        let other = Envelope::new("request.qualified", "i", "CS-2", json!({"text": "hi"})).unwrap();
        assert_eq!(console_view(&other)["payload"], json!({"text": "hi"}));
    }
}
