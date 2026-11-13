//! The orchestrator's phase machine: one Redis hash per case, `{scope}case:{id}` (7 days), plus the scope's
//! deadline set `{scope}cases:deadline` (port of care-bus-agents@96c608c `crates/carebus/src/case.rs`, B4 T7).
//!
//! Hash fields: `request`, `plan` (first plan wins — the plan carries the `CaseDesign` it was read at, so a publish
//! mid-case never changes the design a case runs on), `phase` (the phase index, or `handed_over`: the case stops at
//! its next boundary), `pending:{phase}` (steps still open), `result:{step}`, `call:{call_id}` (status for the
//! sweep), `plan_recorded`, `resolution`, `resolved`, `error`. Every write is safe to repeat: a redelivered message replays the
//! handler that made it.
//!
//! B4 adds `identity` (the level the case runs at and the party it is bound to, [`CaseStore::set_identity`]).
//!
//! Postgres stays the record (`agent_case`, `case_event`); this hash is only the in-flight state.
use std::time::Duration;

use anyhow::{Context, Result};
use redis::{AsyncCommands, Script};
use serde_json::{Map, Value};

use super::bus::Bus;
use super::domain::{Plan, QualifiedRequest, Resolution};

/// Store a result once and count the phase down once. Returns the remaining count, or -1 when this result was
/// already recorded.
const RECORD_RESULT: &str = r#"
if redis.call('HSETNX', KEYS[1], ARGV[1], ARGV[2]) == 1 then
  return redis.call('HINCRBY', KEYS[1], ARGV[3], -1)
end
return -1
"#;

/// Move `phase` from ARGV[1] to ARGV[2] only if it is still ARGV[1] (a handed-over case never moves again).
const ADVANCE_PHASE: &str = r#"
if redis.call('HGET', KEYS[1], 'phase') == ARGV[1] then
  redis.call('HSET', KEYS[1], 'phase', ARGV[2])
  return 1
end
return 0
"#;

/// Create the case hash and its deadline entry in one step, so a crash cannot leave a case with no deadline.
/// KEYS: case hash, deadline zset, root active-scope set. ARGV: request json, due ms, ttl s, case id, scope text.
/// Returns 1 if created.
const CREATE_CASE: &str = r#"
if redis.call('HSETNX', KEYS[1], 'request', ARGV[1]) == 1 then
  redis.call('HSETNX', KEYS[1], 'phase', 0)
  redis.call('EXPIRE', KEYS[1], ARGV[3])
  redis.call('ZADD', KEYS[2], 'NX', ARGV[2], ARGV[4])
  redis.call('SADD', KEYS[3], ARGV[5])
  return 1
end
return 0
"#;

/// Reopen a RESOLVED case for a later message of its thread (A4 `case_per_thread`): the new request replaces the
/// old, `resolved` goes, the phase moves one on (a fresh assistant round: its claim, pending list and call ids are
/// new; the earlier rounds' results stay), and the case is tracked again. Returns 1 when reopened; 0 for an unknown,
/// unresolved or handed-over case.
/// KEYS: case hash, deadline zset. ARGV: request json, due ms, ttl s, case id.
const REOPEN_CASE: &str = r#"
if redis.call('HEXISTS', KEYS[1], 'request') == 0 then return 0 end
if redis.call('HEXISTS', KEYS[1], 'resolved') == 0 then return 0 end
local phase = redis.call('HGET', KEYS[1], 'phase')
if phase == 'handed_over' then return 0 end
redis.call('HSET', KEYS[1], 'request', ARGV[1])
redis.call('HDEL', KEYS[1], 'resolved')
redis.call('HDEL', KEYS[1], 'resolution')
redis.call('HINCRBY', KEYS[1], 'round', 1)
redis.call('HSET', KEYS[1], 'phase', tostring((tonumber(phase) or 0) + 1))
redis.call('EXPIRE', KEYS[1], ARGV[3])
redis.call('ZADD', KEYS[2], ARGV[2], ARGV[4])
return 1
"#;

/// Lease an overdue case: only if its score is due, push the score `lease` ms out and return 1.
/// KEYS: deadline zset. ARGV: case id, now ms, lease ms.
const LEASE_OVERDUE: &str = r#"
local s = redis.call('ZSCORE', KEYS[1], ARGV[1])
if s and tonumber(s) <= tonumber(ARGV[2]) then
  redis.call('ZADD', KEYS[1], tonumber(ARGV[2]) + tonumber(ARGV[3]), ARGV[1])
  return 1
end
return 0
"#;

/// Raise `identity` to ARGV[1] (never lower) and bind ARGV[2] as the party once (an empty ARGV[2] binds none).
/// KEYS: case hash. ARGV[3]: the hash TTL when it has none. Returns the stored JSON.
const SET_IDENTITY: &str = r#"
local raw = redis.call('HGET', KEYS[1], 'identity')
local level = tonumber(ARGV[1])
local party = ''
if raw then
  local cur = cjson.decode(raw)
  level = math.max(tonumber(cur.level) or 0, level)
  if type(cur.party) == 'string' then party = cur.party end
end
if party == '' then party = ARGV[2] end
local out
if party == '' then
  out = cjson.encode({level = level})
else
  out = cjson.encode({level = level, party = party})
end
redis.call('HSET', KEYS[1], 'identity', out)
if redis.call('TTL', KEYS[1]) < 0 then redis.call('EXPIRE', KEYS[1], ARGV[3]) end
return out
"#;

/// How long a replica owns the sweep of one case; a crashed sweep is retried after this.
pub const SWEEP_LEASE: Duration = Duration::from_secs(30);

/// Default case deadline (`CASE_DEADLINE_MS`): an unresolved case older than this ends `failed`. It outlasts the
/// assistant turn ([`crate::runtime::assistant::TURN_BUDGET`], 1,800 s; slow local-llm, owner 2026-11-03).
pub const DEFAULT_CASE_DEADLINE: Duration = Duration::from_secs(3600);

/// How long a case hash lives (7 days).
pub const CASE_TTL_SECS: i64 = 7 * 86_400;

/// The `phase` value of a case that left the agent's hands (staff, or the runner's handover).
pub const HANDED_OVER: &str = "handed_over";

/// The root SET of scopes (`<tenant>/<job>/<env>`) that ever had a tracked case: the offboard sweep's work list
/// (crate::runtime::orchestrator::cancel_offboarded).
pub const ACTIVE_SCOPES_KEY: &str = "orchestrator:scopes";

/// Where a case is in its plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CasePhase {
    At(usize),
    HandedOver,
}

#[derive(Clone)]
pub struct CaseStore {
    bus: Bus,
    deadline: Duration,
}

impl CaseStore {
    /// On a scoped bus (every key is the scope's).
    pub fn new(bus: Bus) -> Self {
        Self {
            bus,
            deadline: DEFAULT_CASE_DEADLINE,
        }
    }

    pub fn with_deadline(mut self, deadline: Duration) -> Self {
        self.deadline = deadline;
        self
    }

    pub fn deadline(&self) -> Duration {
        self.deadline
    }

    /// The scope's sorted set of unresolved case ids, scored by deadline (epoch ms).
    pub fn deadline_key(&self) -> String {
        self.bus.streams.key("cases:deadline")
    }

    pub fn key(&self, case_id: &str) -> String {
        self.bus.streams.key(&format!("case:{case_id}"))
    }

    /// Redis server time in epoch ms, so replicas agree despite clock skew.
    pub async fn now_ms(&self) -> Result<i64> {
        let (secs, micros): (i64, i64) =
            redis::cmd("TIME").query_async(&mut self.bus.conn()).await?;
        Ok(secs * 1000 + micros / 1000)
    }

    /// Unresolved cases whose deadline is at or before `now_ms` (oldest first).
    pub async fn overdue(&self, now_ms: i64, limit: usize) -> Result<Vec<String>> {
        Ok(self
            .bus
            .conn()
            .zrangebyscore_limit(
                self.deadline_key(),
                "-inf",
                now_ms,
                0,
                isize::try_from(limit).unwrap_or(isize::MAX),
            )
            .await?)
    }

    /// Every tracked (unresolved) case id of the scope.
    pub async fn tracked(&self) -> Result<Vec<String>> {
        Ok(self.bus.conn().zrange(self.deadline_key(), 0, -1).await?)
    }

    /// Lease an overdue case for [`SWEEP_LEASE`]: true for exactly one caller. The entry stays in the set until the
    /// case is resolved, so a sweep that dies mid-way is retried when the lease runs out.
    pub async fn claim_overdue(&self, case_id: &str, now_ms: i64) -> Result<bool> {
        let leased: i64 = Script::new(LEASE_OVERDUE)
            .key(self.deadline_key())
            .arg(case_id)
            .arg(now_ms)
            .arg(i64::try_from(SWEEP_LEASE.as_millis()).unwrap_or(i64::MAX))
            .invoke_async(&mut self.bus.conn())
            .await?;
        Ok(leased == 1)
    }

    /// Stop tracking a case (resolved, or its hash is gone).
    pub async fn drop_deadline(&self, case_id: &str) -> Result<()> {
        let _: i64 = self.bus.conn().zrem(self.deadline_key(), case_id).await?;
        Ok(())
    }

    /// Returns false when the case already existed (a duplicate delivery). Hash, phase, expiry, deadline entry and
    /// the root active-scope entry are written by one Lua script.
    pub async fn create(&self, req: &QualifiedRequest) -> Result<bool> {
        let scope = self
            .bus
            .scope()
            .context("the case store needs a scoped bus")?
            .to_string();
        let due =
            self.now_ms().await? + i64::try_from(self.deadline.as_millis()).unwrap_or(i64::MAX);
        let created: i64 = Script::new(CREATE_CASE)
            .key(self.key(&req.case_id))
            .key(self.deadline_key())
            .key(self.bus.root_key(ACTIVE_SCOPES_KEY))
            .arg(serde_json::to_string(req)?)
            .arg(due)
            .arg(CASE_TTL_SECS)
            .arg(&req.case_id)
            .arg(scope)
            .invoke_async(&mut self.bus.conn())
            .await?;
        Ok(created == 1)
    }

    /// A4 `case_per_thread`: `req` is a LATER message (another `message_id`) of a resolved case this store already
    /// holds: reopen it so the orchestrator runs the next round on the new message. False for a duplicate delivery
    /// of the request the case holds, an unresolved or unknown case.
    pub async fn reopen(&self, req: &QualifiedRequest) -> Result<bool> {
        let Some(held) = self.request(&req.case_id).await? else {
            return Ok(false);
        };
        if held.message_id == req.message_id {
            return Ok(false);
        }
        let due =
            self.now_ms().await? + i64::try_from(self.deadline.as_millis()).unwrap_or(i64::MAX);
        let reopened: i64 = Script::new(REOPEN_CASE)
            .key(self.key(&req.case_id))
            .key(self.deadline_key())
            .arg(serde_json::to_string(req)?)
            .arg(due)
            .arg(CASE_TTL_SECS)
            .arg(&req.case_id)
            .invoke_async(&mut self.bus.conn())
            .await?;
        Ok(reopened == 1)
    }

    /// How often the case was reopened for a later message of its thread (A4 `case_per_thread`; 0 = never).
    pub async fn round(&self, case_id: &str) -> Result<u32> {
        let n: Option<u32> = self.bus.conn().hget(self.key(case_id), "round").await?;
        Ok(n.unwrap_or(0))
    }

    /// The request the case was created from (`None` once its hash expired or was never made).
    pub async fn request(&self, case_id: &str) -> Result<Option<QualifiedRequest>> {
        let raw: Option<String> = self.bus.conn().hget(self.key(case_id), "request").await?;
        raw.map(|r| serde_json::from_str(&r))
            .transpose()
            .map_err(Into::into)
    }

    pub async fn plan(&self, case_id: &str) -> Result<Option<Plan>> {
        let raw: Option<String> = self.bus.conn().hget(self.key(case_id), "plan").await?;
        raw.map(|r| serde_json::from_str(&r))
            .transpose()
            .map_err(Into::into)
    }

    /// First plan wins, so two orchestrators racing on one case agree; returns the stored plan.
    pub async fn set_plan(&self, case_id: &str, plan: &Plan) -> Result<Plan> {
        let _: bool = self
            .bus
            .conn()
            .hset_nx(self.key(case_id), "plan", serde_json::to_string(plan)?)
            .await?;
        self.plan(case_id).await?.context("plan vanished")
    }

    /// The plan is on the `agent_case` row (`plan_recorded`): a crash between the plan and its row write is
    /// repaired by the redelivery.
    pub async fn plan_recorded(&self, case_id: &str) -> Result<bool> {
        Ok(self
            .bus
            .conn()
            .hexists(self.key(case_id), "plan_recorded")
            .await?)
    }

    pub async fn mark_plan_recorded(&self, case_id: &str) -> Result<()> {
        let _: bool = self
            .bus
            .conn()
            .hset_nx(self.key(case_id), "plan_recorded", 1)
            .await?;
        Ok(())
    }

    /// The case's deadline (epoch ms) while it is tracked.
    pub async fn due_ms(&self, case_id: &str) -> Result<Option<i64>> {
        let s: Option<f64> = self.bus.conn().zscore(self.deadline_key(), case_id).await?;
        // scores are whole milliseconds, well inside f64's exact integer range
        Ok(s.map(|v| v as i64))
    }

    pub async fn phase(&self, case_id: &str) -> Result<CasePhase> {
        let p: Option<String> = self.bus.conn().hget(self.key(case_id), "phase").await?;
        Ok(match p.as_deref() {
            Some(HANDED_OVER) => CasePhase::HandedOver,
            Some(n) => CasePhase::At(n.parse().unwrap_or(0)),
            None => CasePhase::At(0),
        })
    }

    /// Move the case from phase `from` to `to`; false when another handler moved it first (or it was handed over).
    pub async fn advance_phase(&self, case_id: &str, from: usize, to: usize) -> Result<bool> {
        let moved: i64 = Script::new(ADVANCE_PHASE)
            .key(self.key(case_id))
            .arg(from)
            .arg(to)
            .invoke_async(&mut self.bus.conn())
            .await?;
        Ok(moved == 1)
    }

    /// Mark the case handed over (`phase=handed_over`): the orchestrator stops it at its next boundary. For the
    /// writers of a handover outside the orchestrator (runner `unknown_outcome`, staff hand-over); the orchestrator
    /// also reads `agent_case.status` at every boundary, so this is the fast path, not the only one.
    pub async fn mark_handed_over(&self, case_id: &str) -> Result<()> {
        let _: () = self
            .bus
            .conn()
            .hset(self.key(case_id), "phase", HANDED_OVER)
            .await?;
        Ok(())
    }

    pub async fn init_pending(&self, case_id: &str, phase: usize, steps: usize) -> Result<()> {
        let _: bool = self
            .bus
            .conn()
            .hset_nx(self.key(case_id), format!("pending:{phase}"), steps)
            .await?;
        Ok(())
    }

    /// Steps of `phase` still open (`i64::MAX` before the phase started).
    pub async fn remaining(&self, case_id: &str, phase: usize) -> Result<i64> {
        let r: Option<i64> = self
            .bus
            .conn()
            .hget(self.key(case_id), format!("pending:{phase}"))
            .await?;
        Ok(r.unwrap_or(i64::MAX))
    }

    pub async fn has_result(&self, case_id: &str, step: &str) -> Result<bool> {
        Ok(self
            .bus
            .conn()
            .hexists(self.key(case_id), format!("result:{step}"))
            .await?)
    }

    /// Store `step`'s output once and count `phase` down once; the remaining count, or -1 for a duplicate.
    pub async fn record_result(
        &self,
        case_id: &str,
        phase: usize,
        step: &str,
        output: &Value,
    ) -> Result<i64> {
        let remaining: i64 = Script::new(RECORD_RESULT)
            .key(self.key(case_id))
            .arg(format!("result:{step}"))
            .arg(serde_json::to_string(output)?)
            .arg(format!("pending:{phase}"))
            .invoke_async(&mut self.bus.conn())
            .await?;
        Ok(remaining)
    }

    /// Every recorded step output, by step name.
    pub async fn results(&self, case_id: &str) -> Result<Map<String, Value>> {
        let all: Vec<(String, String)> = self.bus.conn().hgetall(self.key(case_id)).await?;
        let mut out = Map::new();
        for (field, raw) in all {
            if let Some(name) = field.strip_prefix("result:") {
                out.insert(name.into(), serde_json::from_str(&raw)?);
            }
        }
        Ok(out)
    }

    /// The argument context of the case: `request` (slots + `customer_id`) plus every step output by name.
    pub async fn context(&self, req: &QualifiedRequest) -> Result<Map<String, Value>> {
        let mut ctx = self.results(&req.case_id).await?;
        let mut request: Map<String, Value> = req
            .slots
            .iter()
            .filter(|(_, v)| !v.trim().is_empty())
            .map(|(k, v)| (k.clone(), Value::String(v.clone())))
            .collect();
        if let Some(c) = &req.customer_id {
            request.insert("customer_id".into(), Value::String(c.clone()));
        }
        // B4 known customer: the system slots resolved from the customer's own records (`resolve`) are server-side
        // truth and OVERRIDE any value the request carries for the same slot
        for (k, v) in self.resolved_slots(&req.case_id).await? {
            request.insert(k, Value::String(v));
        }
        // B4 known customer: what the context phase loaded (orders, payments, deliveries, cases), for slot resolution
        let loaded = match self.customer_context_raw(&req.case_id).await? {
            Some(raw) => serde_json::from_str::<Value>(&raw).ok(),
            None => None,
        };
        // the known customer's own id is `$party_id` for a step that reads their records by party (`delivery.list`):
        // never a slot the customer is asked for, and never a value a request carries
        if let Some(party) = loaded
            .as_ref()
            .and_then(|v| v.get("party"))
            .and_then(Value::as_str)
            .filter(|p| !p.is_empty())
        {
            request.insert("party_id".into(), Value::String(party.to_owned()));
        }
        ctx.insert("request".into(), Value::Object(request));
        if let Some(v) = loaded {
            ctx.insert("customer_context".into(), v);
        }
        Ok(ctx)
    }

    /// The system slots resolved for the case (`slots:resolved`, written once by the orchestrator's `slots` step);
    /// empty before that or when nothing was resolved.
    pub async fn resolved_slots(
        &self,
        case_id: &str,
    ) -> Result<std::collections::BTreeMap<String, String>> {
        let raw: Option<String> = self
            .bus
            .conn()
            .hget(self.key(case_id), "slots:resolved")
            .await?;
        Ok(raw
            .and_then(|r| serde_json::from_str(&r).ok())
            .unwrap_or_default())
    }

    /// The case's loaded customer context (JSON, field `ctx`), if the context phase ran.
    pub async fn customer_context_raw(&self, case_id: &str) -> Result<Option<String>> {
        Ok(self.bus.conn().hget(self.key(case_id), "ctx").await?)
    }

    /// The status of one dispatched call (`{"call_id","step","operation","phase","attempt","status"}`).
    pub async fn set_call_status(
        &self,
        case_id: &str,
        call_id: &str,
        status: &Value,
    ) -> Result<()> {
        let _: () = self
            .bus
            .conn()
            .hset(
                self.key(case_id),
                format!("call:{call_id}"),
                status.to_string(),
            )
            .await?;
        Ok(())
    }

    /// Every dispatched call with its latest status.
    pub async fn calls(&self, case_id: &str) -> Result<Vec<Value>> {
        let all: Vec<(String, String)> = self.bus.conn().hgetall(self.key(case_id)).await?;
        all.into_iter()
            .filter(|(f, _)| f.starts_with("call:"))
            .map(|(_, raw)| serde_json::from_str(&raw).map_err(Into::into))
            .collect()
    }

    /// The raw infrastructure error that failed the case (operators only; never published).
    pub async fn set_error(&self, case_id: &str, error: &str) -> Result<()> {
        let _: () = self
            .bus
            .conn()
            .hset(self.key(case_id), "error", error)
            .await?;
        Ok(())
    }

    pub async fn error(&self, case_id: &str) -> Result<Option<String>> {
        Ok(self.bus.conn().hget(self.key(case_id), "error").await?)
    }

    /// First resolution wins; returns the stored one, which every caller must publish.
    pub async fn set_resolution(&self, case_id: &str, res: &Resolution) -> Result<Resolution> {
        let mut c = self.bus.conn();
        let key = self.key(case_id);
        let _: bool = c
            .hset_nx(&key, "resolution", serde_json::to_string(res)?)
            .await?;
        let raw: String = c.hget(&key, "resolution").await?;
        Ok(serde_json::from_str(&raw)?)
    }

    pub async fn is_resolved(&self, case_id: &str) -> Result<bool> {
        Ok(self
            .bus
            .conn()
            .hexists(self.key(case_id), "resolved")
            .await?)
    }

    /// B4 customer verification: raise the case's level (never lower) and bind its verified party (first wins).
    pub async fn set_identity(&self, case_id: &str, level: u8, party: Option<&str>) -> Result<()> {
        let _: String = Script::new(SET_IDENTITY)
            .key(self.key(case_id))
            .arg(level)
            .arg(party.unwrap_or(""))
            .arg(CASE_TTL_SECS)
            .invoke_async(&mut self.bus.conn())
            .await?;
        Ok(())
    }

    /// The level a verify raised the case to and the party it bound (`None`: no verify matched; the case runs at
    /// its qualified level, and the runner reads `agent_case` on every call anyway).
    pub async fn identity(&self, case_id: &str) -> Result<Option<(u8, Option<String>)>> {
        let raw: Option<String> = self.bus.conn().hget(self.key(case_id), "identity").await?;
        let Some(raw) = raw else { return Ok(None) };
        let v: Value = serde_json::from_str(&raw)?;
        let level = v
            .get("level")
            .and_then(Value::as_u64)
            .and_then(|l| u8::try_from(l).ok())
            .unwrap_or(0);
        let party = v.get("party").and_then(Value::as_str).map(str::to_owned);
        Ok(Some((level, party)))
    }

    /// The case is done for the orchestrator: no further dispatch, and the deadline entry is dropped.
    pub async fn mark_resolved(&self, case_id: &str, status: &str) -> Result<()> {
        let _: bool = self
            .bus
            .conn()
            .hset_nx(self.key(case_id), "resolved", status)
            .await?;
        self.drop_deadline(case_id).await
    }
}
