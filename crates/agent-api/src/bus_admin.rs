//! Bus admin (B4 T14, ported from care-bus-agents@96c608c `crates/carebus-api/src/redis_admin.rs` and
//! `routes/agents.rs`): what the J23 agents page and the J24 buses page read from Redis, and the operator's writes
//! (claim stuck entries, replay or discard a dead letter). Also the T19 gauges agent-api exports from Redis.
//!
//! Keys in and out of this module are UNPREFIXED (`bus:op`, `bus:dead`); the scope prefix is added here. Every read
//! goes through the request's SCOPED bus (`root.scoped(&scope_of(job, env))`), so another tenant's, job's or env's
//! keys can be neither listed nor addressed. The root keys read here are the heartbeats (`hb:{role}:{consumer}`)
//! and boots (`starts:{role}`) of the runtime processes, which every scope shares (one set of processes, R5); only
//! the `serving` entries of the job's own two scopes are ever returned.
//!
//! K50: an entry's envelope is shown as the console shows it (`bus::console_view`: ids and codes only for `op.*`).
use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap};

use chrono::{DateTime, Utc};
use redis::RedisResult;
use redis::streams::{
    StreamClaimReply, StreamInfoConsumersReply, StreamInfoGroupsReply, StreamPendingCountReply,
    StreamRangeReply,
};
use serde::Serialize;
use serde_json::{Map, Value, json};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::conversations::iso_ms;
use crate::design::{DesignOf, JobRef};
use crate::error::{ApiError, ApiResult};
use crate::providers::llm::{self, Use};
use crate::runtime::bus::{self, Bus, Envelope, groups, now_ms, topics};
use crate::runtime::scope::{Env, Scope};
use crate::runtime::{heartbeat, reply_gate, scopes};
use crate::state::AppState;
use crate::tenancy::JobCtx;

/// `XADD MAXLEN ~` of every stream (`Bus`, 03 §1.3).
pub const MAXLEN: u64 = 100_000;
/// An agent is `warn` from this lag on (`cb:agents.rs:17`).
pub const LAG_WARN: u64 = 20;
/// An agent is `warn` from this many pending entries on (`cb:agents.rs:18`).
pub const PENDING_WARN: u64 = 10;
/// `claimStuck` takes over entries idle at least this long.
pub const CLAIM_IDLE_MS: u64 = 30_000;
/// The consumer `claimStuck` holds entries on (at most [`CLAIM_COUNT`] per call) only while it dead-letters them
/// ([`claim`]); nothing is ever left pending on it.
pub const CLAIM_CONSUMER: &str = "agent-api-admin";
/// The dead-letter reason of an entry `claimStuck` took off its group ([`reason_code`] `stuck`).
pub const STUCK_REASON: &str = "stuck: idle at least 30 s with no live consumer to reclaim it; moved to dead letters by an operator (claimStuck)";
const CLAIM_COUNT: u64 = 100;
/// The dead-letter stream (unprefixed).
pub const DEAD: &str = "bus:dead";
/// The window `busFlow` averages its rates over (minutes).
pub const WINDOW_MIN: u64 = 5;
/// Seconds after a publish or draft save until every agent serves it: re-read 30 s + restart 5 s (`agentFleet`).
pub const FOLLOW_S: u64 = 35;
/// The most entries one window read looks at per stream.
const WINDOW_COUNT: u64 = 10_000;
/// The runtime roles with a consumer group (role = group = heartbeat role).
pub const ROLES: [&str; 3] = [groups::INTERACTION, groups::ORCHESTRATOR, groups::RUNNER];

/// 503 `unavailable`: the runtime bus (Redis) is not configured or not reachable.
pub fn unavailable() -> ApiError {
    ApiError::Unavailable(
        "unavailable",
        "The runtime bus is unavailable right now; try again shortly.".into(),
        Some(5),
    )
}

/// A Redis failure as [`unavailable`] (only the error kind is logged).
pub fn redis_failed(e: redis::RedisError) -> ApiError {
    tracing::warn!(kind = ?e.kind(), "bus_admin_redis_failed");
    unavailable()
}

/// The root bus, or [`unavailable`] while `REDIS_URL` is unset.
pub fn root(st: &AppState) -> ApiResult<&Bus> {
    st.redis.as_ref().ok_or_else(unavailable)
}

/// The scope of the request's job in `env`.
pub fn scope_of(j: &JobCtx, env: Env) -> ApiResult<Scope> {
    Scope::new(j.t.tenant, j.job_key.clone(), env).map_err(ApiError::Internal)
}

/// `?env=`: absent or empty → `prod`; else `prod` | `test`, or 422 `invalid_query`.
pub fn env_param(raw: Option<&str>) -> ApiResult<Env> {
    match raw.map(str::trim) {
        None | Some("") => Ok(Env::Prod),
        Some(s) => Env::parse(s)
            .ok_or_else(|| ApiError::Unprocessable("invalid_query", "env is prod or test.".into())),
    }
}

/// A stream key without the scope prefix, as every route reports it.
pub fn strip<'a>(bus: &Bus, key: &'a str) -> &'a str {
    key.strip_prefix(bus.streams.prefix()).unwrap_or(key)
}

/// `<ms>` or `<ms>-<seq>`, digits only: the only ids XRANGE/XDEL are ever given.
pub fn valid_stream_id(id: &str) -> bool {
    let mut parts = id.splitn(2, '-');
    let digits = |s: &str| !s.is_empty() && s.len() <= 20 && s.bytes().all(|b| b.is_ascii_digit());
    parts.next().is_some_and(digits) && parts.next().is_none_or(digits)
}

/// `(ms, seq)` of a stream id (`seq` 0 when absent).
fn id_parts(id: &str) -> Option<(u64, u64)> {
    let (ms, seq) = id.split_once('-').unwrap_or((id, "0"));
    Some((ms.parse().ok()?, seq.parse().ok()?))
}

/// Stream ids in numeric order (an unparseable id sorts first).
fn id_cmp(a: &str, b: &str) -> Ordering {
    id_parts(a).cmp(&id_parts(b))
}

/// Only `{prefix}bus:*` keys that exist as streams are served.
pub async fn owns(bus: &Bus, unprefixed: &str) -> RedisResult<bool> {
    if !unprefixed.starts_with("bus:") {
        return Ok(false);
    }
    let t: String = redis::cmd("TYPE")
        .arg(bus.streams.key(unprefixed))
        .query_async(&mut bus.conn())
        .await?;
    Ok(t == "stream")
}

/// The scope's `bus:*` streams (unprefixed), sorted.
pub async fn list_streams(bus: &Bus) -> RedisResult<Vec<String>> {
    let mut c = bus.conn();
    let pattern = bus.streams.key("bus:*");
    let (mut cursor, mut out) = (0u64, Vec::new());
    loop {
        let (next, keys): (u64, Vec<String>) = redis::cmd("SCAN")
            .arg(cursor)
            .arg("MATCH")
            .arg(&pattern)
            .arg("TYPE")
            .arg("stream")
            .arg("COUNT")
            .arg(500)
            .query_async(&mut c)
            .await?;
        out.extend(keys.into_iter().map(|k| strip(bus, &k).to_string()));
        if next == 0 {
            break;
        }
        cursor = next;
    }
    out.sort();
    out.dedup(); // SCAN may return a key more than once
    Ok(out)
}

/// One consumer group of a stream (XINFO GROUPS).
#[derive(Clone, Debug, Serialize)]
pub struct GroupInfo {
    pub name: String,
    pub consumers: u64,
    pub pending: u64,
    pub lag: Option<u64>,
    pub last_delivered_id: String,
}

/// A missing key reads as "nothing there" (it was trimmed or deleted between SCAN and XINFO).
fn no_such_key(e: &redis::RedisError) -> bool {
    e.to_string().contains("no such key")
}

/// The groups of the stream `full` (prefixed key).
pub async fn groups_of(bus: &Bus, full: &str) -> RedisResult<Vec<GroupInfo>> {
    let r: RedisResult<StreamInfoGroupsReply> = redis::cmd("XINFO")
        .arg("GROUPS")
        .arg(full)
        .query_async(&mut bus.conn())
        .await;
    let reply = match r {
        Ok(g) => g,
        Err(e) if no_such_key(&e) => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    Ok(reply
        .groups
        .into_iter()
        .map(|g| GroupInfo {
            name: g.name,
            consumers: g.consumers as u64,
            pending: g.pending as u64,
            lag: g.lag.map(|l| l as u64),
            last_delivered_id: g.last_delivered_id,
        })
        .collect())
}

/// The consumer names of `group` on `full`.
async fn consumers_of(bus: &Bus, full: &str, group: &str) -> RedisResult<Vec<String>> {
    let r: RedisResult<StreamInfoConsumersReply> = redis::cmd("XINFO")
        .arg("CONSUMERS")
        .arg(full)
        .arg(group)
        .query_async(&mut bus.conn())
        .await;
    match r {
        Ok(c) => Ok(c.consumers.into_iter().map(|c| c.name).collect()),
        Err(e) if no_such_key(&e) || e.code() == Some("NOGROUP") => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

/// The id-time (ms) of the oldest entry `group` holds pending on `full`; `None` when nothing is pending.
async fn oldest_pending_ms(bus: &Bus, full: &str, group: &str) -> RedisResult<Option<u64>> {
    let r: RedisResult<StreamPendingCountReply> = redis::cmd("XPENDING")
        .arg(full)
        .arg(group)
        .arg("-")
        .arg("+")
        .arg(1)
        .query_async(&mut bus.conn())
        .await;
    match r {
        Ok(p) => Ok(p
            .ids
            .first()
            .and_then(|i| id_parts(&i.id))
            .map(|(ms, _)| ms)),
        Err(e) if no_such_key(&e) || e.code() == Some("NOGROUP") => Ok(None),
        Err(e) => Err(e),
    }
}

async fn xlen(bus: &Bus, full: &str) -> RedisResult<u64> {
    redis::cmd("XLEN")
        .arg(full)
        .query_async(&mut bus.conn())
        .await
}

/// `(entry id, envelope)` of a range reply; entries without a parseable envelope are skipped.
fn parsed(reply: StreamRangeReply) -> Vec<(String, Envelope)> {
    reply
        .ids
        .into_iter()
        .filter_map(|e| {
            let raw: String = e.get("envelope")?;
            match serde_json::from_str::<Envelope>(&raw) {
                Ok(env) => Some((e.id, env)),
                Err(_) => {
                    tracing::warn!(entry = %e.id, "entry without a parseable envelope skipped");
                    None
                }
            }
        })
        .collect()
}

/// The entries of `full` published at or after `since_ms` (at most [`WINDOW_COUNT`]).
async fn window(bus: &Bus, full: &str, since_ms: u64) -> RedisResult<Vec<(String, Envelope)>> {
    let reply: StreamRangeReply = redis::cmd("XRANGE")
        .arg(full)
        .arg(since_ms)
        .arg("+")
        .arg("COUNT")
        .arg(WINDOW_COUNT)
        .query_async(&mut bus.conn())
        .await?;
    Ok(parsed(reply))
}

/// One page of a stream: `(entry id, envelope)` newest first, and the cursor of the next page.
pub type EntryPage = (Vec<(String, Envelope)>, Option<String>);

/// What [`find_dead`] found: the scoped bus holding the dead letter, its env and the dead letter.
pub type FoundDead = (Bus, Env, Envelope);

/// An entry as the routes show it (`Entry`, `types/runtime.ts`): unprefixed stream, console view of the envelope.
pub fn entry_json(stream: &str, entry_id: &str, env: &Envelope) -> Value {
    json!({"stream": stream, "entry_id": entry_id, "envelope": bus::console_view(env)})
}

/// Newest first. `before` is an exclusive stream id. Returns the entries and the cursor: the id of the last RAW row
/// read when the page is full (an unparseable entry is skipped but still advances the cursor).
pub async fn entries_page(
    bus: &Bus,
    key: &str,
    count: usize,
    before: Option<&str>,
) -> RedisResult<EntryPage> {
    let end = match before {
        Some(b) => format!("({b}"),
        None => "+".to_string(),
    };
    let reply: StreamRangeReply = redis::cmd("XREVRANGE")
        .arg(bus.streams.key(key))
        .arg(end)
        .arg("-")
        .arg("COUNT")
        .arg(count)
        .query_async(&mut bus.conn())
        .await?;
    let raw_len = reply.ids.len();
    let last_raw = reply.ids.last().map(|i| i.id.clone());
    let cursor = if raw_len == count { last_raw } else { None };
    Ok((parsed(reply), cursor))
}

/// One entry by id (`XRANGE key id id`); `None` when absent or unparseable.
pub async fn entry_by_id(bus: &Bus, key: &str, id: &str) -> RedisResult<Option<Envelope>> {
    let reply: StreamRangeReply = redis::cmd("XRANGE")
        .arg(bus.streams.key(key))
        .arg(id)
        .arg(id)
        .query_async(&mut bus.conn())
        .await?;
    Ok(parsed(reply).into_iter().next().map(|(_, e)| e))
}

/// `claimStuck`: take the entries of `group` on `key` idle ≥ [`CLAIM_IDLE_MS`] and owned by no live consumer off the group and onto `bus:dead`,
/// each with [`STUCK_REASON`] (or the reason a consumer would have given: `unparseable envelope`,
/// `scope_mismatch: …`); how many it moved. `None` when the group is missing (`NOGROUP`).
///
/// Operator-triggered only (`POST …/streams/{key}/groups/{group}/claim`, routes::streams); no sweeper or timer calls
/// it. Caveat: [`CLAIM_IDLE_MS`] is a fixed 30 s, shorter than the runner's and orchestrator's own `reclaim_idle`
/// (`ConsumeOptions::for_handler_budget`: 610 / 610 s, a slow model call is a healthy 30 s+ idle). The heartbeat check
/// below is what keeps a live slow handler's entry safe; an operator must not run this against a role whose
/// beat is absent for less than its handler budget.
///
/// Why dead letters and not a live consumer: every live consumer of the group XAUTOCLAIMs entries idle ≥ its
/// `reclaim_idle` (30 s by default, longer for the runner and orchestrator) itself on every turn of its loop (`Bus::consume`), so an entry still idle past that has no live
/// consumer to hand it to (its role is down, or the handler is wedged). Parking it on a consumer that never reads
/// (the old XAUTOCLAIM to [`CLAIM_CONSUMER`]) only hid it for another 30 s. On `bus:dead` the operator sees it,
/// and `replayDeadLetter` puts it back on its stream (a fresh entry, delivery count 0) once the role is serving,
/// or `discardDeadLetter` drops it. [`CLAIM_CONSUMER`] holds an entry only between the XAUTOCLAIM (which keeps a
/// live consumer from taking it meanwhile) and its dead-letter ack; if agent-api dies in between, the entry is
/// idle again 30 s later and a live consumer reclaims it as usual.
///
/// An entry whose owner still beats (`hb:{group}:{consumer}`, [`heartbeat`]) is left alone: a handler runs for as
/// long as its LLM / tool calls take, and while it does its consumer reads nothing (idle > 30 s) yet the process
/// beats from its own task, so idle alone cannot tell a wedged entry from one being worked on. The owner reclaims
/// it itself if the handler fails; if the process dies the beat expires (60 s) and the next claim takes it.
pub async fn claim(bus: &Bus, key: &str, group: &str) -> ApiResult<Option<usize>> {
    let stream = bus.streams.key(key);
    let mut ids: Vec<String> = Vec::new();
    let mut start = "-".to_string();
    'pages: loop {
        let page: StreamPendingCountReply = match redis::cmd("XPENDING")
            .arg(&stream)
            .arg(group)
            .arg("IDLE")
            .arg(CLAIM_IDLE_MS)
            .arg(&start)
            .arg("+")
            .arg(CLAIM_COUNT)
            .query_async(&mut bus.conn())
            .await
        {
            Ok(p) => p,
            Err(e) if e.code() == Some("NOGROUP") => return Ok(None),
            Err(e) => return Err(redis_failed(e)),
        };
        let full = page.ids.len() as u64 == CLAIM_COUNT;
        let last = page.ids.last().map(|i| i.id.clone());
        for p in &page.ids {
            let alive: bool = redis::cmd("EXISTS")
                .arg(heartbeat::key(bus, group, &p.consumer))
                .query_async(&mut bus.conn())
                .await
                .map_err(redis_failed)?;
            // Known tradeoff: an owner whose heartbeat is live is skipped, so a consumer that is wedged but whose
            // heartbeat task still beats is never reclaimed here. Accepted: idle alone cannot tell it from a handler
            // that is slowly working, and reclaiming a live owner's entry would double-process it.
            if !alive {
                ids.push(p.id.clone());
                if ids.len() as u64 == CLAIM_COUNT {
                    break 'pages;
                }
            }
        }
        match last {
            Some(l) if full => start = format!("({l}"),
            _ => break,
        }
    }
    if ids.is_empty() {
        return Ok(Some(0));
    }
    // XCLAIM re-checks the idle time, so an entry its owner finished or touched since the XPENDING is skipped
    let r: StreamClaimReply = match redis::cmd("XCLAIM")
        .arg(&stream)
        .arg(group)
        .arg(CLAIM_CONSUMER)
        .arg(CLAIM_IDLE_MS)
        .arg(&ids)
        .query_async(&mut bus.conn())
        .await
    {
        Ok(r) => r,
        Err(e) if e.code() == Some("NOGROUP") => return Ok(None),
        Err(e) => return Err(redis_failed(e)),
    };
    for entry in &r.ids {
        bus.dead_letter_taken_over(&stream, group, entry, STUCK_REASON)
            .await
            .map_err(|e| {
                tracing::warn!(error = %format!("{e:#}"), stream = %stream, group, entry = %entry.id, "claim_stuck_dead_letter_failed");
                unavailable()
            })?;
    }
    Ok(Some(r.ids.len()))
}

/// XDEL; the count of entries actually removed (the replay/discard gate).
pub async fn delete(bus: &Bus, key: &str, id: &str) -> RedisResult<u64> {
    redis::cmd("XDEL")
        .arg(bus.streams.key(key))
        .arg(id)
        .query_async(&mut bus.conn())
        .await
}

// ---- dead letters ---------------------------------------------------------------------------------------------

/// The `reason_code` of a dead letter's `reason` (`types/runtime.ts DeadLetter`), from the texts `Bus` writes.
pub fn reason_code(reason: &str) -> &'static str {
    if reason.starts_with("scope_mismatch") {
        "scope_mismatch"
    } else if reason.starts_with("gave up") {
        "gave_up"
    } else if reason.starts_with("unparseable") {
        "decode_error"
    } else if reason.starts_with("stuck") {
        "stuck"
    } else {
        "other"
    }
}

/// A dead letter as `deadLetters` shows it (`DeadLetter`), in the scope of `bus` (`env`).
pub fn dead_json(bus: &Bus, env: Env, entry_id: &str, dl: &Envelope) -> Value {
    let p = &dl.payload;
    let origin = p["stream"].as_str().unwrap_or_default();
    let reason = p["reason"].as_str().unwrap_or_default();
    let mut v = entry_json(DEAD, entry_id, dl);
    v["env"] = json!(env);
    v["dead"] = json!({
        "stream": strip(bus, origin),
        "entry_id": p["entry_id"],
        "deliveries": p["deliveries"],
        "reason_code": reason_code(reason),
        "reason": reason,
    });
    v
}

/// The dead letters of both environments of the job, newest first, `limit` per page. `before` is an exclusive
/// stream id; the cursor is the last id shown when more may follow.
pub async fn dead_page(
    root: &Bus,
    j: &JobCtx,
    limit: usize,
    before: Option<&str>,
) -> ApiResult<(Vec<Value>, Option<String>)> {
    let mut rows: Vec<(String, Value)> = Vec::new();
    let mut more = false;
    for env in Env::ALL {
        let bus = root.scoped(&scope_of(j, env)?);
        let (items, cursor) = entries_page(&bus, DEAD, limit, before)
            .await
            .map_err(redis_failed)?;
        more |= cursor.is_some();
        rows.extend(
            items
                .into_iter()
                .map(|(id, dl)| (id.clone(), dead_json(&bus, env, &id, &dl))),
        );
    }
    rows.sort_by(|a, b| id_cmp(&b.0, &a.0));
    more |= rows.len() > limit;
    rows.truncate(limit);
    let next = if more {
        rows.last().map(|(id, _)| id.clone())
    } else {
        None
    };
    Ok((rows.into_iter().map(|(_, v)| v).collect(), next))
}

/// The dead letter `id` of the job (prod first, then test): its scoped bus, env and envelope.
pub async fn find_dead(root: &Bus, j: &JobCtx, id: &str) -> ApiResult<Option<FoundDead>> {
    if !valid_stream_id(id) {
        return Ok(None);
    }
    for env in Env::ALL {
        let bus = root.scoped(&scope_of(j, env)?);
        if let Some(dl) = entry_by_id(&bus, DEAD, id).await.map_err(redis_failed)? {
            return Ok(Some((bus, env, dl)));
        }
    }
    Ok(None)
}

/// 404 `dead_letter_not_found`.
pub fn dead_letter_not_found() -> ApiError {
    ApiError::Missing(
        "dead_letter_not_found",
        "That dead letter is gone (replayed or discarded).".into(),
    )
}

/// 409 `scope_mismatch`.
fn scope_mismatch() -> ApiError {
    ApiError::Conflict(
        "scope_mismatch",
        "This message belongs to another tenant, job or environment. Replaying it would deliver it into the wrong scope — discard it instead."
            .into(),
    )
}

/// What a replay did: the origin stream (unprefixed) and the new entry id.
pub struct Replayed {
    pub stream: String,
    pub entry_id: String,
}

/// Replay the dead letter `id` (found in `bus`) onto its origin stream. 409 `scope_mismatch` for a dead letter of
/// another scope (its reason, or an origin outside this scope's `bus:*`), 409 `not_replayable` when it holds no
/// original envelope, 404 when another replay or a discard removed it first (XDEL is the claim: of two concurrent
/// replays only the one that removes the entry publishes). A failed publish puts the dead letter back.
pub async fn replay(bus: &Bus, id: &str, dl: &Envelope) -> ApiResult<Replayed> {
    let p = &dl.payload;
    if reason_code(p["reason"].as_str().unwrap_or_default()) == "scope_mismatch" {
        return Err(scope_mismatch());
    }
    let origin = p["stream"].as_str().unwrap_or_default().to_string();
    // Only a stream of THIS scope: the scoped prefix is part of the check.
    if !origin.starts_with(&bus.streams.key("bus:")) || origin == bus.streams.dead() {
        return Err(scope_mismatch());
    }
    let inner = serde_json::from_value::<Envelope>(p["original"].clone()).map_err(|_| {
        ApiError::Conflict(
            "not_replayable",
            "This dead letter holds no readable message (it could not be parsed when it failed); discard it."
                .into(),
        )
    })?;
    if delete(bus, DEAD, id).await.map_err(redis_failed)? != 1 {
        return Err(dead_letter_not_found());
    }
    // The scoped publish re-stamps the envelope with this scope.
    let new_id = match bus.publish(&origin, &inner).await {
        Ok(new_id) => new_id,
        Err(e) => {
            tracing::error!(error = %format!("{e:#}"), dead_letter = %id, "replay publish failed; restoring the dead letter");
            if let Err(re) = bus.publish(&bus.streams.dead(), dl).await {
                tracing::error!(error = %format!("{re:#}"), dead_letter = %id, "could not restore the dead letter");
            }
            return Err(unavailable());
        }
    };
    Ok(Replayed {
        stream: strip(bus, &origin).to_string(),
        entry_id: new_id,
    })
}

// ---- J23 agents -----------------------------------------------------------------------------------------------

/// One bus agent of the catalogue: a runtime role, with its own consumer group or (R1) running inside another's.
struct AgentDef {
    id: &'static str,
    name: &'static str,
    kind: &'static str,
    /// The heartbeat role; for a role with a group, also the group.
    role: &'static str,
    /// False for a role without a stream of its own (R1): its numbers come from its live beats ([`beat_totals`]).
    grouped: bool,
    /// LLM uses its work resolves (`model_routes` lists the bound ones).
    uses: &'static [Use],
    note: Option<&'static str>,
}

const AGENTS: [AgentDef; 4] = [
    AgentDef {
        id: "interaction",
        name: "Interaction agent",
        kind: "front",
        role: groups::INTERACTION,
        grouped: true,
        uses: &[Use::Qualify],
        note: None,
    },
    AgentDef {
        id: "orchestrator",
        name: "Orchestrator",
        kind: "coordination",
        role: groups::ORCHESTRATOR,
        grouped: true,
        uses: &[],
        note: None,
    },
    AgentDef {
        id: "runner",
        name: "Operation runner",
        kind: "runner",
        role: groups::RUNNER,
        grouped: true,
        uses: &[],
        note: None,
    },
    AgentDef {
        id: "post_gate",
        name: "Post-gate (R1 fact check)",
        kind: "gate",
        role: reply_gate::ROLE,
        grouped: false,
        uses: &[Use::Gates],
        note: Some(
            "Runs inside the interaction daemon: checks each reply against knowledge before it is stored. \
             No stream of its own, so no pending or lag.",
        ),
    },
];

/// A stream of a scope with its consumer groups (key unprefixed).
struct StreamGroups {
    key: String,
    groups: Vec<GroupInfo>,
}

async fn scope_groups(bus: &Bus) -> RedisResult<Vec<StreamGroups>> {
    let mut out = Vec::new();
    for key in list_streams(bus).await? {
        let groups = groups_of(bus, &bus.streams.key(&key)).await?;
        out.push(StreamGroups { key, groups });
    }
    Ok(out)
}

/// A group's numbers over a scope's streams. One process joins the same group on several streams: consumers and
/// lag are the max, pending the sum.
#[derive(Default)]
struct Totals {
    present: bool,
    consumers: u64,
    pending: u64,
    lag: u64,
    streams: Vec<String>,
}

fn totals(sg: &[StreamGroups], group: &str) -> Totals {
    let mut t = Totals::default();
    for s in sg {
        for g in s.groups.iter().filter(|g| g.name == group) {
            t.present = true;
            t.consumers = t.consumers.max(g.consumers);
            t.pending += g.pending;
            t.lag = t.lag.max(g.lag.unwrap_or(0));
            t.streams.push(s.key.clone());
        }
    }
    t
}

/// The numbers of a role without a stream of its own: one consumer per live beat, nothing pending, no lag.
fn beat_totals(live: usize) -> Totals {
    Totals {
        present: live > 0,
        consumers: u64::try_from(live).unwrap_or(u64::MAX),
        ..Totals::default()
    }
}

/// `idle` without a consumer, `warn` at lag ≥ 20 or pending ≥ 10, else `ok`.
fn agent_status(t: &Totals) -> &'static str {
    if t.consumers == 0 {
        "idle"
    } else if t.lag >= LAG_WARN || t.pending >= PENDING_WARN {
        "warn"
    } else {
        "ok"
    }
}

/// One live heartbeat (`hb:{role}:{consumer}`).
pub struct Beat {
    pub consumer: String,
    pub started_at: u64,
    pub beat_at: u64,
    pub version: String,
    pub serving: Map<String, Value>,
}

/// Every live heartbeat of `role` (expired ones are gone: a process that stopped beating 60 s ago).
pub async fn beats(root: &Bus, role: &str) -> RedisResult<Vec<Beat>> {
    let mut c = root.conn();
    let pattern = heartbeat::key(root, role, "*");
    let (mut cursor, mut keys) = (0u64, BTreeSet::new());
    loop {
        let (next, found): (u64, Vec<String>) = redis::cmd("SCAN")
            .arg(cursor)
            .arg("MATCH")
            .arg(&pattern)
            .arg("COUNT")
            .arg(500)
            .query_async(&mut c)
            .await?;
        keys.extend(found);
        if next == 0 {
            break;
        }
        cursor = next;
    }
    let mut out = Vec::new();
    for k in keys {
        let h: HashMap<String, String> = redis::cmd("HGETALL").arg(&k).query_async(&mut c).await?;
        let Some(consumer) = h.get("consumer").cloned() else {
            continue; // expired between SCAN and HGETALL
        };
        let num = |f: &str| h.get(f).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
        out.push(Beat {
            consumer,
            started_at: num("started_at"),
            beat_at: num("beat_at"),
            version: h.get("version").cloned().unwrap_or_default(),
            serving: h
                .get("serving")
                .and_then(|s| serde_json::from_str::<Map<String, Value>>(s).ok())
                .unwrap_or_default(),
        });
    }
    out.sort_by_key(|b| std::cmp::Reverse(b.beat_at));
    Ok(out)
}

/// The boots of `role` in the last 24 h (`{consumer}@{ms}` members of `starts:{role}`).
async fn starts(root: &Bus, role: &str, now: u64) -> RedisResult<Vec<String>> {
    let window = u64::try_from(heartbeat::STARTS_WINDOW.as_millis()).unwrap_or(u64::MAX);
    redis::cmd("ZRANGEBYSCORE")
        .arg(heartbeat::starts_key(root, role))
        .arg(now.saturating_sub(window))
        .arg("+inf")
        .query_async(&mut root.conn())
        .await
}

/// `(acked per minute over the last 5 minutes, handled in the last hour, errors in the last hour)` of `group` from
/// the scope's `metrics:{group}:{minute}` hashes (written by `Bus` after each recipient handler).
async fn rates(bus: &Bus, group: &str, now: u64) -> RedisResult<(f64, u64, u64)> {
    let minute = now / 60_000;
    let mut pipe = redis::pipe();
    for m in 0..60 {
        pipe.cmd("HMGET")
            .arg(bus.metrics_key(group, minute.saturating_sub(m)))
            .arg("handled")
            .arg("errors");
    }
    let rows: Vec<Vec<Option<u64>>> = pipe.query_async(&mut bus.conn()).await?;
    let (mut recent, mut handled, mut errors) = (0u64, 0u64, 0u64);
    for (i, r) in rows.iter().enumerate() {
        let h = r.first().copied().flatten().unwrap_or(0);
        if (i as u64) < WINDOW_MIN {
            recent += h;
        }
        handled += h;
        errors += r.get(1).copied().flatten().unwrap_or(0);
    }
    Ok((round1(recent as f64 / WINDOW_MIN as f64), handled, errors))
}

fn round1(n: f64) -> f64 {
    (n * 10.0).round() / 10.0
}

fn iso_of(ms: u64) -> String {
    iso_ms(i64::try_from(ms).unwrap_or(i64::MAX))
}

/// What the newest heartbeat says `scope` serves (`AgentServing` without the numbers).
fn serving_of(beats: &[Beat], scope: &Scope) -> Option<(Value, Value, u64)> {
    let key = scope.to_string();
    beats
        .iter()
        .filter_map(|b| b.serving.get(&key))
        .map(|s| {
            (
                s["release"].clone(),
                s["rev"].clone(),
                s["read_at"].as_u64().unwrap_or(0),
            )
        })
        .max_by_key(|s| s.2)
}

fn serving_json(s: Option<(Value, Value, u64)>, t: &Totals) -> Value {
    let (release, rev, read_at) = match s {
        Some((r, v, at)) => (r, v, Value::String(iso_of(at))),
        None => (Value::Null, Value::Null, Value::Null),
    };
    json!({
        "release": release,
        "rev": rev,
        "read_at": read_at,
        "consumers": t.consumers,
        "pending": t.pending,
        "lag": t.lag,
    })
}

/// The LLM uses of `uses` bound for the job (Live design); a binding that cannot serve counts as unbound.
async fn bound_uses(st: &AppState, j: &JobCtx, uses: &[Use]) -> ApiResult<Vec<String>> {
    if uses.is_empty() {
        return Ok(Vec::new());
    }
    let job = JobRef {
        id: j.job_id,
        key: j.job_key.clone(),
    };
    // read-only: dropped (rolled back) rather than committed, so a refused binding's failed statement cannot fail
    // the whole read
    let mut tx = j.begin(&st.db).await?;
    let mut out = Vec::new();
    for u in uses {
        if let Ok(Some(_)) = llm::resolve(&mut tx, st, j.t.tenant, &job, *u, &DesignOf::Live).await
        {
            out.push(u.as_str().to_string());
        }
    }
    Ok(out)
}

/// `busAgents`: the catalogue × the consumer groups of the job's prod (and test) streams, the processes from the
/// heartbeats, rates from `metrics:*`. The post-gate (R1) has no group: its numbers are its live beats.
pub async fn agents(st: &AppState, j: &JobCtx) -> ApiResult<Vec<Value>> {
    let root_bus = root(st)?;
    let (prod_scope, test_scope) = (scope_of(j, Env::Prod)?, scope_of(j, Env::Test)?);
    let (prod, test) = (root_bus.scoped(&prod_scope), root_bus.scoped(&test_scope));
    let prod_groups = scope_groups(&prod).await.map_err(redis_failed)?;
    let test_groups = scope_groups(&test).await.map_err(redis_failed)?;
    let now = now_ms();
    let mut out = Vec::new();
    for a in &AGENTS {
        let hbs = beats(root_bus, a.role).await.map_err(redis_failed)?;
        let (pt, tt) = if a.grouped {
            (totals(&prod_groups, a.role), totals(&test_groups, a.role))
        } else {
            (beat_totals(hbs.len()), beat_totals(hbs.len()))
        };
        let boots = starts(root_bus, a.role, now).await.map_err(redis_failed)?;
        let mut consumers: BTreeSet<String> = BTreeSet::new();
        let mut oldest: Option<u64> = None;
        for key in &pt.streams {
            let full = prod.streams.key(key);
            consumers.extend(
                consumers_of(&prod, &full, a.role)
                    .await
                    .map_err(redis_failed)?,
            );
            if let Some(ms) = oldest_pending_ms(&prod, &full, a.role)
                .await
                .map_err(redis_failed)?
            {
                oldest = Some(oldest.map_or(ms, |o: u64| o.min(ms)));
            }
        }
        consumers.extend(hbs.iter().map(|b| b.consumer.clone()));
        let boots_of = |c: &str| {
            let p = format!("{c}@");
            let mut v: Vec<u64> = boots
                .iter()
                .filter_map(|m| m.strip_prefix(&p)?.parse().ok())
                .collect();
            v.sort_unstable();
            v
        };
        let instances: Vec<Value> = consumers
            .iter()
            .map(|c| {
                let beat = hbs.iter().find(|b| &b.consumer == c);
                let b = boots_of(c.as_str());
                let started = beat.map(|x| x.started_at).or_else(|| b.last().copied());
                json!({
                    "name": c,
                    "state": if beat.is_some() { "running" } else { "stopped" },
                    "started_at": started.map(iso_of),
                    "restarts": b.len().saturating_sub(1),
                    "last_heartbeat": beat.map(|x| iso_of(x.beat_at)),
                })
            })
            .collect();
        let distinct: BTreeSet<&str> = boots
            .iter()
            .filter_map(|m| m.rsplit_once('@').map(|(c, _)| c))
            .collect();
        let (per_min, handled_1h, errors_1h) =
            rates(&prod, a.role, now).await.map_err(redis_failed)?;
        let test_serving = serving_of(&hbs, &test_scope);
        let test_json = if tt.present || test_serving.is_some() {
            serving_json(test_serving, &tt)
        } else {
            Value::Null
        };
        let mut agent = json!({
            "id": a.id,
            "name": a.name,
            "kind": a.kind,
            "role": a.role,
            "group": a.role,
            "consumers": pt.consumers,
            "pending": pt.pending,
            "lag": pt.lag,
            "streams": pt.streams,
            "status": agent_status(&pt),
            "model_routes": bound_uses(st, j, a.uses).await?,
            "last_heartbeat": hbs.first().map(|b| iso_of(b.beat_at)),
            "version": hbs.first().map(|b| b.version.clone()).unwrap_or_default(),
            "instances": instances,
            "per_min": per_min,
            "handled_1h": handled_1h,
            "errors_1h": errors_1h,
            "restarts_24h": boots.len().saturating_sub(distinct.len()),
            "oldest_pending_s": oldest.map(|ms| now.saturating_sub(ms) / 1000),
            "prod": serving_json(serving_of(&hbs, &prod_scope), &pt),
            "test": test_json,
        });
        if let Some(note) = a.note {
            agent["note"] = Value::String(note.into());
        }
        out.push(agent);
    }
    Ok(out)
}

/// `agentFleet`: what Production (the current release) and the Test env (the draft revision and its unpublished
/// changes) should serve, and the approved release waiting to be applied, as of now.
pub async fn fleet(c: &mut PgConnection, job: &JobRef) -> ApiResult<Value> {
    fleet_at(c, job, Utc::now()).await
}

/// [`fleet`] stamped `as_of`: the ONE `AgentFleet` computation, also the `fleet` of `caseload` (routes/cases.rs),
/// which stamps it with its own transaction's `now()`.
pub async fn fleet_at(
    c: &mut PgConnection,
    job: &JobRef,
    as_of: DateTime<Utc>,
) -> ApiResult<Value> {
    let release = crate::release::summary::current_ref(&mut *c, job).await?;
    let released_at = sqlx::query_scalar::<_, Option<DateTime<Utc>>>(
        "SELECT r.released_at FROM jobs j \
           JOIN job_release r ON r.tenant_id = j.tenant_id AND r.job_id = j.id \
                             AND r.release_id = j.current_release_id \
          WHERE j.id = $1",
    )
    .bind(job.id)
    .fetch_optional(&mut *c)
    .await?
    .flatten();
    let rev = crate::design::draft_rev(&mut *c, job).await?;
    let unpublished: i64 =
        sqlx::query_scalar("SELECT count(*) FROM draft_ledger WHERE job_id = $1")
            .bind(job.id)
            .fetch_one(&mut *c)
            .await?;
    // `releaseStatus.approved_waiting` (release::summary::status): the newest release when it is an approved cut
    let newest: Option<(String, String)> = sqlx::query_as(
        "SELECT release_id, state FROM job_release WHERE job_id = $1 \
          ORDER BY approved_at DESC, release_id DESC LIMIT 1",
    )
    .bind(job.id)
    .fetch_optional(&mut *c)
    .await?;
    let waiting = newest
        .filter(|(_, state)| state == "approved")
        .map(|(id, _)| id);
    Ok(json!({
        "as_of": crate::conversations::iso(as_of),
        "production": {
            "release": release.map(|r| json!({"id": r.id, "hash": r.hash})),
            "released_at": released_at.map(crate::conversations::iso),
        },
        "test": {"rev": rev, "unpublished": unpublished},
        "approved_waiting": waiting,
        "follow_s": FOLLOW_S,
    }))
}

// ---- J24 bus flow ---------------------------------------------------------------------------------------------

/// `bus` of a `StreamInfo` / `TopicInfo` by key (`op` for the operation streams, R1).
fn bus_kind(key: &str) -> &'static str {
    let parts: Vec<&str> = key.split(':').collect();
    match parts.as_slice() {
        ["bus", "customer", _] => "customer",
        ["bus", "op"] | ["bus", "op", "results"] => "op",
        ["bus", "dead"] => "dead",
        _ => "other",
    }
}

/// `(events, producers)` of a stream by key.
fn topic_meta(key: &str) -> (&'static [&'static str], &'static [&'static str]) {
    let parts: Vec<&str> = key.split(':').collect();
    match parts.as_slice() {
        ["bus", "customer", _] => (
            &[topics::REQUEST_QUALIFIED, topics::REQUEST_RESOLVED],
            &["agent-api", groups::ORCHESTRATOR],
        ),
        ["bus", "op"] => (&[topics::OP_CALL], &[groups::ORCHESTRATOR]),
        ["bus", "op", "results"] => (&[topics::OP_RESULT], &[groups::RUNNER]),
        ["bus", "dead"] => (&[topics::DEAD_LETTER], &["any consumer"]),
        _ => (&[], &[]),
    }
}

/// `err` at lag > 20 or pending > 10, `warn` at lag > 5 or pending > 3 (`labels.ts groupHealth`).
fn group_health(g: &GroupInfo) -> &'static str {
    let lag = g.lag.unwrap_or(0);
    if lag > 20 || g.pending > 10 {
        "err"
    } else if lag > 5 || g.pending > 3 {
        "warn"
    } else {
        "ok"
    }
}

fn rank(s: &str) -> u8 {
    match s {
        "ok" => 1,
        "warn" => 2,
        "err" => 3,
        _ => 0,
    }
}

fn worst<'a>(xs: impl IntoIterator<Item = &'a str>) -> &'a str {
    xs.into_iter()
        .fold("ok", |a, b| if rank(b) > rank(a) { b } else { a })
}

/// Idle when nobody consumes, else the worst group (`J24-flow.ts groupsStatus`).
fn groups_status(gs: &[&GroupInfo]) -> &'static str {
    if gs.is_empty() {
        "ok"
    } else if gs.iter().all(|g| g.consumers == 0) {
        "idle"
    } else {
        worst(gs.iter().map(|g| group_health(g)))
    }
}

/// p95 of `xs` (ms), `None` for none.
fn p95(mut xs: Vec<u64>) -> Option<u64> {
    if xs.is_empty() {
        return None;
    }
    xs.sort_unstable();
    let i = ((xs.len() as f64) * 0.95).ceil() as usize;
    xs.get(i.saturating_sub(1)).copied()
}

/// What `busFlow` read from one stream.
struct Topic {
    key: String,
    groups: Vec<GroupInfo>,
    length: u64,
    /// Entries in the window by event.
    events: HashMap<&'static str, u64>,
    total: u64,
}

/// `busFlow`: the fixed topology of the B4 runtime with live rates over [`WINDOW_MIN`] minutes, and the topics
/// behind it, for one environment of the job.
pub async fn flow(root: &Bus, j: &JobCtx, env: Env) -> ApiResult<Value> {
    let scope = scope_of(j, env)?;
    let bus = root.scoped(&scope);
    let now = now_ms();
    let since = now.saturating_sub(WINDOW_MIN * 60_000);
    let mut read: Vec<Topic> = Vec::new();
    let mut latencies: Vec<u64> = Vec::new();
    let mut outcomes: HashMap<String, u64> = HashMap::new();
    let mut dead_by_origin: HashMap<String, u64> = HashMap::new();
    for key in list_streams(&bus).await.map_err(redis_failed)? {
        let full = bus.streams.key(&key);
        let groups = groups_of(&bus, &full).await.map_err(redis_failed)?;
        let length = xlen(&bus, &full).await.map_err(redis_failed)?;
        let mut events: HashMap<&'static str, u64> = HashMap::new();
        let mut total = 0u64;
        if key == DEAD {
            let all: StreamRangeReply = redis::cmd("XRANGE")
                .arg(&full)
                .arg("-")
                .arg("+")
                .arg("COUNT")
                .arg(WINDOW_COUNT)
                .query_async(&mut bus.conn())
                .await
                .map_err(redis_failed)?;
            for (_, dl) in parsed(all) {
                let origin = dl.payload["stream"].as_str().unwrap_or_default();
                *dead_by_origin
                    .entry(strip(&bus, origin).to_string())
                    .or_insert(0) += 1;
            }
        }
        for (_, e) in window(&bus, &full, since).await.map_err(redis_failed)? {
            total += 1;
            let Some(ev) = e.event() else { continue };
            *events.entry(ev).or_insert(0) += 1;
            if ev == topics::OP_RESULT
                && let Some(ms) = e.payload["latency_ms"].as_u64()
            {
                latencies.push(ms);
            }
            if ev == topics::REQUEST_RESOLVED
                && let Some(o) = e.payload["outcome"].as_str()
            {
                *outcomes.entry(o.to_string()).or_insert(0) += 1;
            }
        }
        read.push(Topic {
            key,
            groups,
            length,
            events,
            total,
        });
    }
    let per_min = |n: u64| round1(n as f64 / WINDOW_MIN as f64);
    let live = read
        .iter()
        .any(|t| t.groups.iter().any(|g| g.consumers > 0));
    let topics_json: Vec<Value> = read
        .iter()
        .map(|t| {
            let (events, producers) = topic_meta(&t.key);
            json!({
                "key": t.key,
                "bus": bus_kind(&t.key),
                "events": events,
                "producers": producers,
                "groups": t.groups,
                "rate_per_min": per_min(t.total),
                "length": t.length,
                "maxlen": MAXLEN,
                "dead": dead_by_origin.get(&t.key).copied().unwrap_or(0),
            })
        })
        .collect();
    let is_cust = |t: &&Topic| bus_kind(&t.key) == "customer";
    let is_op = |t: &&Topic| t.key == "bus:op";
    let is_results = |t: &&Topic| t.key == "bus:op:results";
    let keys = |f: &dyn Fn(&&Topic) -> bool| -> Vec<String> {
        read.iter().filter(f).map(|t| t.key.clone()).collect()
    };
    let count = |ev: &str| -> u64 {
        read.iter()
            .map(|t| t.events.get(ev).copied().unwrap_or(0))
            .sum()
    };
    let topic_st = |f: &dyn Fn(&&Topic) -> bool| -> &'static str {
        if !live {
            return "idle";
        }
        let sts: Vec<&'static str> = read
            .iter()
            .filter(f)
            .map(|t| groups_status(&t.groups.iter().collect::<Vec<_>>()))
            .collect();
        worst(sts)
    };
    let agent_st = |group: &str| -> &'static str {
        if !live {
            return "idle";
        }
        let gs: Vec<&GroupInfo> = read
            .iter()
            .flat_map(|t| t.groups.iter().filter(|g| g.name == group))
            .collect();
        groups_status(&gs)
    };
    let qualified = per_min(count(topics::REQUEST_QUALIFIED));
    let resolved = per_min(count(topics::REQUEST_RESOLVED));
    let calls = per_min(count(topics::OP_CALL));
    let results = per_min(count(topics::OP_RESULT));
    let exit = |outcome: &str| per_min(outcomes.get(outcome).copied().unwrap_or(0));
    let test = env == Env::Test;
    let idle_or = |s: &'static str| if live { s } else { "idle" };
    let hops = if read.is_empty() {
        Vec::new()
    } else {
        vec![
            json!({
                "id": "channel-in", "phase": "in", "kind": "channel",
                "label": if test { "Test chat (staff)" } else { "Channels" },
                "does": if test {
                    "Staff test chats against the draft. Nothing reaches a customer."
                } else {
                    "Customer messages from the job's channels and the staff chat land on agent-api, deduplicated and drained through the licence gate."
                },
                "streams": [], "events": ["customer message"], "agents": ["agent-api"],
                "rate_per_min": qualified, "p95_ms": null,
                "link": if test { json!({"href": "/chat", "label": "Test chat"}) } else { json!({"href": "/admin/channels", "label": "Channels"}) },
                "status": idle_or("ok"),
            }),
            json!({
                "id": "qualify", "phase": "in", "kind": "gate", "label": "Qualifier",
                "does": "Qualifies the message with the job's qualify model and opens the case before publishing.",
                "streams": [], "events": [], "agents": ["agent-api"],
                "rate_per_min": qualified, "p95_ms": null,
                "link": {"href": "/gates", "label": "Gates"},
                "status": idle_or("ok"),
            }),
            json!({
                "id": "qualified", "phase": "in", "kind": "topic", "label": topics::REQUEST_QUALIFIED,
                "does": "One entry per qualified request; the case id is the correlation id from here on.",
                "streams": keys(&is_cust), "events": [topics::REQUEST_QUALIFIED], "agents": [],
                "rate_per_min": qualified, "p95_ms": null,
                "status": topic_st(&is_cust),
            }),
            json!({
                "id": "orchestrator", "phase": "work", "kind": "agent", "label": "Orchestrator · procedure",
                "does": if test {
                    "Opens the case, picks the DRAFT procedure and plans its steps."
                } else {
                    "Opens the case, picks the procedure of the live release and plans its steps."
                },
                "streams": [], "events": [], "agents": [groups::ORCHESTRATOR],
                "rate_per_min": qualified, "p95_ms": null,
                "link": {"href": "/procedures", "label": "Procedures"},
                "status": agent_st(groups::ORCHESTRATOR),
            }),
            json!({
                "id": "op", "phase": "work", "kind": "topic", "label": "op.call ⇄ op.result",
                "does": "Each step of the procedure becomes one operation call for the runner; its answer comes back on bus:op:results.",
                "streams": keys(&|t: &&Topic| is_op(t) || is_results(t)), "events": [topics::OP_CALL, topics::OP_RESULT],
                "agents": [], "rate_per_min": calls, "p95_ms": null, "loop": true,
                "status": topic_st(&|t: &&Topic| is_op(t) || is_results(t)),
            }),
            json!({
                "id": "runner", "phase": "work", "kind": "agent", "label": "Operation runner",
                "does": if test {
                    "Runs each call on the test-env route (stub or test instance)."
                } else {
                    "Runs each call on its route: a built-in, the agent's own cases, or a connector instance."
                },
                "streams": [], "events": [], "agents": [groups::RUNNER],
                "rate_per_min": results, "p95_ms": p95(latencies),
                "link": {"href": "/operation-calls", "label": "Operation calls"},
                "status": agent_st(groups::RUNNER),
            }),
            json!({
                "id": "resolved", "phase": "out", "kind": "topic", "label": topics::REQUEST_RESOLVED,
                "does": "The orchestrator publishes the case's outcome on the customer topic.",
                "streams": keys(&is_cust), "events": [topics::REQUEST_RESOLVED], "agents": [],
                "rate_per_min": resolved, "p95_ms": null,
                "exits": [
                    {"label": "deflected (not eligible)", "per_min": exit("not_eligible"), "tone": "warn"},
                    {"label": "handed over to staff", "per_min": exit("handover"), "tone": "warn"},
                    {"label": "failed", "per_min": exit("failed"), "tone": "err"},
                ],
                "link": {"href": "/handover", "label": "Handover queue"},
                "status": topic_st(&is_cust),
            }),
            json!({
                "id": "interaction", "phase": "out", "kind": "agent", "label": "Interaction agent",
                "does": "Composes the reply from the outcome and its facts and appends it to the conversation.",
                "streams": [], "events": [], "agents": [groups::INTERACTION],
                "rate_per_min": resolved, "p95_ms": null,
                "status": agent_st(groups::INTERACTION),
            }),
            json!({
                "id": "reply", "phase": "out", "kind": "channel",
                "label": if test { "Reply → test chat" } else { "Reply → customer" },
                "does": if test {
                    "The reply appears in the staff test chat; the case is recorded as env=test with the draft revision."
                } else {
                    "The reply is delivered on the conversation's channel."
                },
                "streams": [], "events": ["reply"], "agents": [groups::INTERACTION],
                "rate_per_min": resolved, "p95_ms": null,
                "link": {"href": "/cases", "label": "Cases"},
                "status": agent_st(groups::INTERACTION),
            }),
        ]
    };
    let dead_letters: u64 = read
        .iter()
        .filter(|t| t.key == DEAD)
        .map(|t| t.length)
        .sum();
    Ok(json!({
        "env": env,
        "prefix": scope.prefix(""),
        "reads": if test { "draft" } else { "live" },
        "window_min": WINDOW_MIN,
        "hops": hops,
        "topics": topics_json,
        "dead_letters": dead_letters,
    }))
}

// ---- T19 gauges -----------------------------------------------------------------------------------------------

/// The Redis-sourced runtime numbers, summed over every registered scope (labels never carry a tenant).
#[derive(Debug, Default, PartialEq)]
pub struct Gauges {
    /// Pending entries by consumer group.
    pub pending: HashMap<&'static str, u64>,
    /// Lag by consumer group.
    pub lag: HashMap<&'static str, u64>,
    pub dead_letters: u64,
    /// Open handover items past a due time (first response or resolve).
    pub handover_breached: i64,
    /// Seconds since the freshest heartbeat of each role; `None` = no live process.
    pub heartbeat_age_s: HashMap<&'static str, Option<f64>>,
}

/// Open PRODUCTION handover items of `tenant` past a due time (first response or resolve), RLS-scoped.
///
/// `env = 'prod'` only (D54): a Test-env item comes from the staff test chat trying a draft — no customer waits on
/// it, so counting it fired `AgentHandoverBreached` ("a customer is waiting on staff") for design-time test chats
/// nobody is meant to staff (2026-10-23: 13 of 16 open items on the demo tenant were `env = 'test'`).
pub async fn count_breached(db: &PgPool, tenant: Uuid) -> anyhow::Result<i64> {
    let mut tx = crate::tenancy::begin_scoped(db, tenant)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM agent_handover_items \
          WHERE closed_at IS NULL AND env = 'prod' \
            AND ((first_response_at IS NULL AND first_response_due < now()) OR resolve_due < now())",
    )
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(n)
}

/// Read [`Gauges`]: the registry's scopes (Redis), their streams, the heartbeats, and the breached handover items
/// of the registry's tenants (one RLS-scoped read per tenant).
pub async fn read_gauges(db: &PgPool, root: &Bus) -> anyhow::Result<Gauges> {
    let mut g = Gauges::default();
    for r in ROLES {
        g.pending.insert(r, 0);
        g.lag.insert(r, 0);
    }
    let all = scopes::list(root).await?;
    let mut tenants: BTreeSet<Uuid> = BTreeSet::new();
    for s in &all {
        tenants.insert(s.tenant);
        let bus = root.scoped(s);
        for key in list_streams(&bus).await? {
            let full = bus.streams.key(&key);
            if key == DEAD {
                g.dead_letters += xlen(&bus, &full).await?;
                continue;
            }
            for gi in groups_of(&bus, &full).await? {
                if let Some(r) = ROLES.into_iter().find(|r| *r == gi.name) {
                    *g.pending.entry(r).or_insert(0) += gi.pending;
                    *g.lag.entry(r).or_insert(0) += gi.lag.unwrap_or(0);
                }
            }
        }
    }
    for t in tenants {
        g.handover_breached += count_breached(db, t).await?;
    }
    let now = now_ms();
    for r in ROLES {
        let age = beats(root, r)
            .await?
            .iter()
            .map(|b| now.saturating_sub(b.beat_at) as f64 / 1000.0)
            .reduce(f64::min);
        g.heartbeat_age_s.insert(r, age);
    }
    Ok(g)
}

/// Read the gauges and set them on the process's recorder: `agent_bus_pending{group}`, `agent_bus_lag{group}`,
/// `agent_dead_letters`, `agent_handover_breached`, `agent_heartbeat_age_seconds{role}` (+Inf with no live process,
/// so an absent runtime is loud, never a stale 0).
pub async fn refresh_gauges(db: &PgPool, root: &Bus) -> anyhow::Result<()> {
    let g = read_gauges(db, root).await?;
    for (group, n) in &g.pending {
        metrics::gauge!("agent_bus_pending", "group" => *group).set(*n as f64);
    }
    for (group, n) in &g.lag {
        metrics::gauge!("agent_bus_lag", "group" => *group).set(*n as f64);
    }
    metrics::gauge!("agent_dead_letters").set(g.dead_letters as f64);
    metrics::gauge!("agent_handover_breached").set(g.handover_breached as f64);
    for (role, age) in &g.heartbeat_age_s {
        metrics::gauge!("agent_heartbeat_age_seconds", "role" => *role)
            .set(age.unwrap_or(f64::INFINITY));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_ids_and_reason_codes() {
        assert!(valid_stream_id("1700000000000-0"));
        assert!(valid_stream_id("17"));
        assert!(!valid_stream_id("nope"));
        assert!(!valid_stream_id("1-"));
        assert!(!valid_stream_id("-1"));
        assert_eq!(id_cmp("9-0", "10-0"), Ordering::Less);
        assert_eq!(id_cmp("10-10", "10-9"), Ordering::Greater);
        assert_eq!(reason_code("scope_mismatch: stamped x"), "scope_mismatch");
        assert_eq!(reason_code("gave up after 5 deliveries"), "gave_up");
        assert_eq!(reason_code("unparseable envelope"), "decode_error");
        assert_eq!(reason_code("something else"), "other");
    }

    #[test]
    fn topology_kinds_and_health() {
        assert_eq!(bus_kind("bus:customer:returns"), "customer");
        assert_eq!(bus_kind("bus:op"), "op");
        assert_eq!(bus_kind("bus:op:results"), "op");
        assert_eq!(bus_kind("bus:dead"), "dead");
        assert_eq!(topic_meta("bus:op:results").1, &[groups::RUNNER]);
        let g = |consumers, pending, lag| GroupInfo {
            name: "g".into(),
            consumers,
            pending,
            lag,
            last_delivered_id: "0-0".into(),
        };
        assert_eq!(groups_status(&[&g(0, 50, Some(50))]), "idle");
        assert_eq!(groups_status(&[&g(1, 0, Some(0))]), "ok");
        assert_eq!(groups_status(&[&g(1, 4, Some(0))]), "warn");
        assert_eq!(groups_status(&[&g(1, 0, Some(21)), &g(1, 4, None)]), "err");
        assert_eq!(p95(vec![]), None);
        assert_eq!(p95((1..=100).collect()), Some(95));
        assert_eq!(p95(vec![7]), Some(7));
    }

    #[test]
    fn agent_status_warns_at_the_cb_thresholds() {
        let t = |consumers, pending, lag| Totals {
            present: true,
            consumers,
            pending,
            lag,
            streams: vec![],
        };
        assert_eq!(agent_status(&t(0, 99, 99)), "idle");
        assert_eq!(agent_status(&t(1, 9, 19)), "ok");
        assert_eq!(agent_status(&t(1, 10, 0)), "warn");
        assert_eq!(agent_status(&t(1, 0, 20)), "warn");
    }

    #[test]
    fn a_role_without_a_stream_reads_its_live_beats() {
        assert!(!beat_totals(0).present);
        assert_eq!(agent_status(&beat_totals(0)), "idle");
        let t = beat_totals(2);
        assert!(t.present);
        assert_eq!((t.consumers, t.pending, t.lag), (2, 0, 0));
        assert!(t.streams.is_empty());
        assert_eq!(agent_status(&t), "ok");
        let gate = AGENTS.iter().find(|a| a.id == "post_gate").unwrap();
        assert_eq!(
            (gate.kind, gate.role, gate.grouped),
            ("gate", reply_gate::ROLE, false)
        );
        assert!(
            AGENTS
                .iter()
                .filter(|a| a.grouped)
                .all(|a| ROLES.contains(&a.role))
        );
    }
}
