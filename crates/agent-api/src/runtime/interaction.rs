//! The interaction daemon (B4 T8): consumes `request.resolved` (group `interaction`) on the scope's
//! `bus:customer:{area}` streams and answers the customer, inline (no spawned task per reply).
//!
//! One resolution, delivered at least once:
//!
//! 1. Test env only: the [`ReplyTrace`] is built first, in its own read-only transaction (a trace read never holds
//!    a lock and never fails the reply; its error code lands in `unavailable`).
//! 2. One transaction (job-scoped, as `agent_runtime` in the cluster), under the conversation's control lock
//!    (`store::control_key`, the lock Take / Assign / Hand back / console replies take):
//!    - the reply already stored (its `external_id` is `reply:{case}`) → nothing is written (a redelivery);
//!    - else, anyone but the agent owns the conversation (SW-1: a staff member, or the queue of an open handover
//!      item — except the case's own item of a `handover` or `failed` resolution with no handover notice stored for
//!      the case yet, see `withheld`) → a `state` case event "agent reply withheld" and nothing for the customer;
//!    - else `conversations::store::append` (role `agent`), the `reply` case event (message id + trace), and
//!      `knowledge::analytics::record_grounding` for that message (§0 R3: a reply and its grounding commit
//!      together, or neither does); when the case ended `resolved` (SW-6): `conversations::csat::arm` the
//!      conversation's one CSAT request, unsent (`awaiting_confirmation`) — no prompt line, no `csat` frame: the
//!      prompt goes out when the customer's next message acknowledges the resolution. Not armed on a channel nobody
//!      reads (`api`, `mcp`, `voice`, `staff`), a binding with CSAT off, a reply that offers a colleague or asks a
//!      question, or after a complaint or a customer-asked handover (`csat::suppressed`, counted).
//! 3. After commit, Lua [`CLAIM_AND_RELAY`]: `SET replied:{case} NX EX 604800` and, only for the first claim,
//!    PUBLISH the frames (`message`; a `csat` frame only for a stored prompt line, which this daemon no longer writes)
//!    on the scope's `chat` channel ([`ChatFrame`], bridged into the SSE hub by agent-api). The claim comes after the commit, so a crash anywhere leaves either no reply
//!    (redelivery writes it) or a stored reply (redelivery relays it once): the claim only ever gates the publish.
//!
//! B5's R1 (crate::runtime::reply_gate) runs before the append when the daemon has it. Customer text is data: it
//! is never logged; the log lines carry case ids and codes only.
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use super::bus::{Bus, ConsumeOptions, Envelope, groups, topics};
use super::design::DesignCache;
use super::domain::{CaseOutcome, Resolution};
use super::heartbeat::Serving;
use super::id_guard::{self, ReplyOrigin};
use super::reply::{self, ReplyExtras};
use super::reply_gate::{self, Gated, ReplyCase, ReplyHook};
use super::scope::{Env, Scope};
use super::trace::{self, ReplyTrace};
use crate::conversations::ownership::{self, Owner};
use crate::conversations::{csat, handover, store};
use crate::design::{JobRef, Registry};
use crate::knowledge::analytics;
use crate::state::AppState;

/// The runtime role name (consumer names, heartbeats).
pub const ROLE: &str = "interaction";

/// How long a case's reply claim lives: seven days (a redelivery inside it never publishes twice).
pub const REPLIED_TTL_SECS: u64 = 604_800;

/// `KEYS[1]` = `replied:{case}`, `KEYS[2]` = the scope's `chat` channel; `ARGV[1]` = the reply's message id,
/// `ARGV[2]` = the claim's TTL, `ARGV[3..]` = the frames. Claims and publishes atomically; 1 = claimed now.
pub const CLAIM_AND_RELAY: &str = r#"
if redis.call('SET', KEYS[1], ARGV[1], 'NX', 'EX', ARGV[2]) then
  for i = 3, #ARGV do
    redis.call('PUBLISH', KEYS[2], ARGV[i])
  end
  return 1
end
return 0
"#;

/// One frame on a scope's `chat` Pub/Sub channel (the shape `crate::sse_hub` bridges into the event hub; tenant and
/// job are the channel's scope).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChatFrame {
    pub conversation_id: String,
    /// `message` | `csat`.
    pub event: String,
    pub data: Value,
}

/// What one resolution did.
#[derive(Clone, Debug, PartialEq)]
pub enum Delivery {
    /// The reply is stored; `relayed` = this delivery published it (false: an earlier delivery had).
    Replied {
        message_id: Uuid,
        written: bool,
        csat: bool,
        relayed: bool,
    },
    /// Nothing reached the customer: `staff_control` or `open_handover`.
    Withheld { reason: &'static str },
}

/// The interaction role of one scope.
pub struct Interaction {
    db: PgPool,
    design: Arc<Registry>,
    bus: Bus,
    scope: Scope,
    /// B5 R1; `None` = every reply is stored as composed.
    pub reply_gate: Option<ReplyHook>,
}

/// The case row the reply needs.
#[derive(sqlx::FromRow)]
struct CaseRow {
    conversation_id: String,
    status: String,
    release_id: Option<String>,
    draft_rev: Option<i64>,
}

/// A stored message the frames are rebuilt from.
#[derive(sqlx::FromRow)]
struct StoredMsg {
    message_id: Uuid,
    at_ms: i64,
    text: String,
}

impl Interaction {
    /// `bus` must be scoped (the supervisor's scope bus).
    pub fn new(db: PgPool, design: Arc<Registry>, bus: Bus) -> Result<Self> {
        let scope = bus
            .scope()
            .cloned()
            .ok_or_else(|| anyhow!("the interaction daemon needs a scoped bus"))?;
        Ok(Self {
            db,
            design,
            bus,
            scope,
            reply_gate: None,
        })
    }

    pub fn scope(&self) -> &Scope {
        &self.scope
    }

    /// The scope's job (404 `job_not_found` when it is gone).
    pub async fn job(&self) -> Result<JobRef> {
        let mut tx = crate::tenancy::begin_scoped(&self.db, self.scope.tenant).await?;
        let id: Option<Uuid> = sqlx::query_scalar("SELECT id FROM jobs WHERE key = $1")
            .bind(&self.scope.job)
            .fetch_optional(&mut *tx)
            .await?;
        tx.commit().await?;
        let id = id.ok_or_else(|| anyhow!("job_not_found: {}", self.scope))?;
        Ok(JobRef {
            id,
            key: self.scope.job.clone(),
        })
    }

    /// Handle one envelope of a customer stream: `request.resolved` is delivered, every other event is not ours.
    pub async fn handle(&self, env: &Envelope) -> Result<Option<Delivery>> {
        if env.event() != Some(topics::REQUEST_RESOLVED) {
            return Ok(None);
        }
        let r: Resolution = env.payload_as()?;
        let extras: ReplyExtras = env.payload_as()?;
        self.deliver(&r, &extras).await.map(Some)
    }

    /// Deliver the reply of resolution `r` (module docs). An `Err` leaves the entry pending for redelivery.
    pub async fn deliver(&self, r: &Resolution, extras: &ReplyExtras) -> Result<Delivery> {
        let job = self.job().await?;
        let trace = match self.scope.env {
            Env::Test => Some(self.trace(&job, r, extras).await?),
            Env::Prod => None,
        };
        let case_id = r.case_id.as_str();
        let mut tx = store::begin_job(&self.db, self.scope.tenant, job.id).await?;
        let case: CaseRow = sqlx::query_as(
            "SELECT conversation_id, status, release_id, draft_rev FROM agent_case WHERE case_id = $1",
        )
        .bind(case_id)
        .fetch_optional(&mut *tx)
        .await?
        .with_context(|| format!("case_not_found: {case_id}"))?;
        let conv = case.conversation_id.as_str();
        // the control lock, then the owner row (ownership::guard, SW T5): R1's reply gate may queue a handover item
        // below, so this transaction locks in the ownership commands' order
        ownership::guard(&mut tx, conv).await?;
        let reply_key = format!("reply:{}", r.reply_ref());
        let csat_key = format!("csat:{}", r.reply_ref());
        let mut queued: Option<String> = None;
        let (reply_msg, written, csat_asked) = match stored(&mut tx, conv, &reply_key).await? {
            Some(m) => (m, false, stored(&mut tx, conv, &csat_key).await?.is_some()),
            None => {
                if let Some(reason) = withheld(&mut tx, conv, r).await? {
                    case_event(
                        &mut tx,
                        job.id,
                        case_id,
                        "state",
                        json!({"note": "agent reply withheld", "reason": reason}),
                    )
                    .await?;
                    tx.commit().await?;
                    tracing::info!(case = case_id, reason, "agent_reply_withheld");
                    return Ok(Delivery::Withheld { reason });
                }
                let (rel, rev) = (case.release_id.clone(), case.draft_rev);
                let rc = ReplyCase::of(&self.scope, &self.design, &job, r, conv, rel, rev);
                match reply_gate::check(self.reply_gate.as_ref(), &mut tx, rc).await? {
                    Some(Gated::Blocked) => {
                        tx.commit().await?;
                        return Ok(Delivery::Withheld {
                            reason: "gate_blocked",
                        });
                    }
                    Some(Gated::Notice(n)) => {
                        queued = n.item;
                        let m = StoredMsg {
                            message_id: n.message_id,
                            at_ms: n.at_ms,
                            text: n.text,
                        };
                        (m, true, false)
                    }
                    None => {
                        let (m, asked) = self
                            .write(&mut tx, &job, r, extras, &case, trace.as_ref())
                            .await?;
                        (m, true, asked)
                    }
                }
            }
        };
        tx.commit().await?;
        if let (Some(item), Some(h)) = (queued, &self.reply_gate) {
            h.notify(self.scope.tenant, job.id, &item).await;
        }
        if written {
            self.observe_ids(&job, conv, case_id, &reply_msg.text).await;
        }
        let out = frames(conv, case_id, &reply_msg, trace.as_ref(), csat_asked);
        let relayed = self
            .claim_and_relay(&r.reply_ref(), reply_msg.message_id, &out)
            .await?;
        Ok(Delivery::Replied {
            message_id: reply_msg.message_id,
            written,
            csat: csat_asked,
            relayed,
        })
    }

    /// The id-shape guard (owner decision 2026-10-27, log-only): a reply written now (a redelivery relays the stored
    /// one and is not checked again) that holds id-shaped text is logged and counted by [`id_guard::observe`]. It
    /// never changes, withholds or hands over the reply; the channel is looked up only on a hit, and a failed lookup
    /// reads `unknown`.
    async fn observe_ids(&self, job: &JobRef, conv: &str, case_id: &str, text: &str) {
        if !id_guard::has_id_shape(text) {
            return;
        }
        let channel = async {
            let mut tx = store::begin_job(&self.db, self.scope.tenant, job.id)
                .await
                .ok()?;
            let found = store::customer_ref(&mut tx, conv).await.ok()??;
            Some(found.0.channel)
        }
        .await
        .unwrap_or_else(|| "unknown".to_owned());
        id_guard::observe(
            &ReplyOrigin {
                tenant: self.scope.tenant,
                job: &self.scope.job,
                conversation: conv,
                case: Some(case_id),
                channel: &channel,
            },
            text,
        );
    }

    /// Store the reply, its timeline event and grounding, and the CSAT request of a resolved case (armed, unsent).
    /// Returns the reply and whether a CSAT prompt was sent (never: `false`).
    async fn write(
        &self,
        tx: &mut PgConnection,
        job: &JobRef,
        r: &Resolution,
        extras: &ReplyExtras,
        case: &CaseRow,
        trace: Option<&ReplyTrace>,
    ) -> Result<(StoredMsg, bool)> {
        let case_id = r.case_id.as_str();
        let conv = case.conversation_id.as_str();
        let reply_key = format!("reply:{}", r.reply_ref());
        let text = reply::compose(r);
        let s = store::append(
            tx,
            store::NewMessage {
                job_id: job.id,
                conversation_id: conv,
                role: "agent",
                author: "agent",
                text: &text,
                case_id: Some(case_id),
                external_id: Some(&reply_key),
            },
        )
        .await?
        .ok_or_else(|| anyhow!("reply_conflict: {case_id}"))?;
        let mut body = json!({"message_id": s.message_id});
        if let Some(t) = trace {
            body["trace"] = serde_json::to_value(t)?;
        }
        case_event(tx, job.id, case_id, "reply", body).await?;
        analytics::record_grounding(
            tx,
            job.id,
            s.message_id,
            &reply::grounding(r, extras.knowledge.as_ref()),
        )
        .await?;
        // SW-6: a resolved, askable reply on a channel a person reads, where the binding allows it and no complaint
        // or customer-asked handover suppresses it, ARMS the conversation's one request. Nothing is sent now (no
        // prompt line, no `csat` frame): the prompt goes out when the customer's next message acknowledges.
        if case.status == "resolved"
            && csat::askable(&text)
            && store::conversation(tx, conv)
                .await?
                .is_some_and(|c| csat::channel_asks(&c.channel))
            && csat::enabled(tx, conv).await?
        {
            match csat::suppressed(tx, conv).await? {
                Some(reason) => csat::count_suppressed(reason),
                None => {
                    csat::arm(tx, case_id).await?;
                }
            }
        }
        let asked = false;
        Ok((
            StoredMsg {
                message_id: s.message_id,
                at_ms: s.at_ms,
                text,
            },
            asked,
        ))
    }

    /// The test-env trace: the orchestrator's input plus the design side, read in its own transaction.
    async fn trace(
        &self,
        job: &JobRef,
        r: &Resolution,
        extras: &ReplyExtras,
    ) -> Result<ReplyTrace> {
        let input = extras.trace.clone().unwrap_or_default();
        let mut tx = store::begin_job(&self.db, self.scope.tenant, job.id).await?;
        let case: Option<CaseRow> = sqlx::query_as(
            "SELECT conversation_id, status, release_id, draft_rev FROM agent_case WHERE case_id = $1",
        )
        .bind(&r.case_id)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        let stamp = match case {
            Some(CaseRow {
                draft_rev: Some(rev),
                ..
            }) => crate::design::CaseDesign::Draft { draft_rev: rev },
            Some(c) => crate::design::CaseDesign::Live {
                release_id: c.release_id,
            },
            None => return Err(anyhow!("case_not_found: {}", r.case_id)),
        };
        let mut t = trace::base(&stamp, &input, &r.facts);
        let mut tx = store::begin_job(&self.db, self.scope.tenant, job.id).await?;
        match trace::add_design(&mut tx, &self.design, job, &input.read, &mut t).await {
            Ok(()) => tx.commit().await?,
            Err(e) => {
                let code = e.code();
                drop(tx);
                tracing::warn!(case = %r.case_id, code, "reply_trace_design_unavailable");
                t.unavailable = Some(code.to_string());
            }
        }
        Ok(t)
    }

    /// Run [`CLAIM_AND_RELAY`] for `case_id`: true when this call claimed the reply and published `frames`.
    pub async fn claim_and_relay(
        &self,
        case_id: &str,
        message_id: Uuid,
        frames: &[ChatFrame],
    ) -> Result<bool> {
        let script = redis::Script::new(CLAIM_AND_RELAY);
        let mut inv = script.prepare_invoke();
        inv.key(replied_key(&self.bus, case_id))
            .key(self.bus.streams.chat())
            .arg(message_id.to_string())
            .arg(REPLIED_TTL_SECS);
        for f in frames {
            inv.arg(serde_json::to_string(f)?);
        }
        let n: i64 = inv
            .invoke_async(&mut self.bus.conn())
            .await
            .context("claim_and_relay")?;
        Ok(n == 1)
    }

    /// The context-space areas the scope's design names (its `bus:customer:{area}` streams).
    pub async fn areas(&self, cache: &DesignCache, serving: &Serving) -> Result<Vec<String>> {
        let job = self.job().await?;
        let mut tx = store::begin_job(&self.db, self.scope.tenant, job.id).await?;
        let d = cache
            .get_or_load(&mut tx, &self.design, &self.scope, &job)
            .await?;
        tx.commit().await?;
        let (release, rev) = store::stamp(&d.case);
        serving.set(&self.scope, release, rev);
        Ok(d.spaces.areas())
    }

    /// Consume `request.resolved` on the scope's customer streams until the scope's areas change (then `Ok`: the
    /// supervisor restarts the scope with the new streams) or a stream fails.
    pub async fn run(
        self: Arc<Self>,
        consumer: &str,
        opts: ConsumeOptions,
        cache: Arc<DesignCache>,
        serving: Serving,
    ) -> Result<()> {
        let areas = self.areas(&cache, &serving).await?;
        let changed = {
            let me = Arc::clone(&self);
            let (cache, serving, areas) = (Arc::clone(&cache), serving.clone(), areas.clone());
            async move {
                loop {
                    tokio::time::sleep(super::design::REFRESH).await;
                    match me.areas(&cache, &serving).await {
                        Ok(now) if now != areas => return,
                        Ok(_) => {}
                        Err(e) => {
                            tracing::warn!(scope = %me.scope, "interaction: design read failed: {e:#}")
                        }
                    }
                }
            }
        };
        if areas.is_empty() {
            tracing::info!(scope = %self.scope, "interaction: no context space yet; waiting for the design");
            changed.await;
            return Ok(());
        }
        let streams: Vec<String> = areas.iter().map(|a| self.bus.streams.customer(a)).collect();
        let me = Arc::clone(&self);
        let handler = move |env: Envelope| {
            let me = Arc::clone(&me);
            async move {
                match me.handle(&env).await {
                    Ok(_) => Ok(()),
                    Err(e) => {
                        tracing::warn!(scope = %me.scope, correlation = %env.correlation_id, "interaction: reply failed: {e:#}");
                        Err(e)
                    }
                }
            }
        };
        tokio::select! {
            r = self.bus.consume_all(streams, groups::INTERACTION, consumer, opts, handler) => r,
            () = changed => Ok(()),
        }
    }
}

/// The `chat` frames of a stored reply (and the CSAT prompt's when it was requested).
fn frames(
    conv: &str,
    case_id: &str,
    m: &StoredMsg,
    trace: Option<&ReplyTrace>,
    csat_asked: bool,
) -> Vec<ChatFrame> {
    let frame = |event: &str, data: Value| ChatFrame {
        conversation_id: conv.to_string(),
        event: event.to_string(),
        data,
    };
    let mut data = json!({
        "conversation_id": conv, "message_id": m.message_id, "role": "agent", "author": "agent",
        "text": m.text, "case_id": case_id, "at_ms": m.at_ms,
    });
    if let Some(t) = trace {
        data["trace"] = serde_json::to_value(t).unwrap_or(Value::Null);
    }
    let mut out = vec![frame("message", data)];
    if csat_asked {
        out.push(frame(
            "csat",
            json!({"conversation_id": conv, "case_id": case_id}),
        ));
    }
    out
}

/// `replied:{case}` of the scope.
pub fn replied_key(bus: &Bus, case_id: &str) -> String {
    bus.streams.key(&format!("replied:{case_id}"))
}

/// Why the agent must not answer resolution `r` now (SW-1): a staff member owns the conversation (`staff_control`),
/// or the queue does (`open_handover`). One open item does not withhold: the one this very case queued when it
/// ended `handover` or `failed` (the orchestrator queues and commits it before the resolution), so the customer
/// hears "passed to a colleague" / "a colleague will follow up" — unless a handover notice was already stored for
/// the case (the customer asked for a person: no second notice).
async fn withheld(
    c: &mut PgConnection,
    conv: &str,
    r: &Resolution,
) -> Result<Option<&'static str>> {
    // SW-1: the agent answers exactly when it owns the conversation
    match ownership::read(&mut *c, conv).await?.owner {
        Owner::Agent => return Ok(None),
        Owner::User(_) => return Ok(Some("staff_control")),
        Owner::Queue => {}
    }
    let Some(item) = handover::open_for_conversation(&mut *c, conv).await? else {
        return Ok(Some("open_handover"));
    };
    // a `failed` case queued its own item too (its reply promises a colleague): the same exemption
    if matches!(r.outcome, CaseOutcome::Handover | CaseOutcome::Failed) && item.case_id == r.case_id
    {
        let noticed: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM conversation_message \
                             WHERE conversation_id = $1 AND case_id = $2 AND author = $3)",
        )
        .bind(conv)
        .bind(&r.case_id)
        .bind(handover::NOTICE_AUTHOR)
        .fetch_one(&mut *c)
        .await?;
        if !noticed {
            return Ok(None);
        }
    }
    Ok(Some("open_handover"))
}

/// The message of `conv` stored with `external_id`.
async fn stored(c: &mut PgConnection, conv: &str, external_id: &str) -> Result<Option<StoredMsg>> {
    Ok(sqlx::query_as(
        "SELECT message_id, at_ms, text FROM conversation_message \
          WHERE conversation_id = $1 AND external_id = $2",
    )
    .bind(conv)
    .bind(external_id)
    .fetch_optional(c)
    .await?)
}

/// Append one case timeline event (append-only; `agent_runtime` may not read it back).
async fn case_event(
    c: &mut PgConnection,
    job_id: Uuid,
    case_id: &str,
    kind: &str,
    body: Value,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO case_event (tenant_id, job_id, case_id, kind, body) \
         VALUES (app_current_tenant(), $1, $2, $3, $4)",
    )
    .bind(job_id)
    .bind(case_id)
    .bind(kind)
    .bind(body)
    .execute(c)
    .await?;
    Ok(())
}

/// The daemon: one [`Interaction`] per registered scope (crate::runtime::supervisor), the design cache following
/// the config nudges, and the heartbeat. Never returns `Ok`.
pub async fn run_daemon(
    db: PgPool,
    design: Arc<Registry>,
    root: Bus,
    st: Option<AppState>,
) -> Result<()> {
    let consumer = super::consumer_name(ROLE);
    let cache = Arc::new(DesignCache::default());
    let _follow = cache.follow(root.clone());
    let serving = Serving::default();
    let _beat = tokio::spawn(super::heartbeat::run(
        root.clone(),
        ROLE.to_string(),
        consumer.clone(),
        serving.clone(),
    ));
    // R1 runs in this process when it has the runtime state (ReplyHook::of); it has no consumer group, so it beats
    // as its own role (`post_gate`), which is how `busAgents` lists it
    let _r1_beat = st.is_some().then(|| {
        tokio::spawn(super::heartbeat::run(
            root.clone(),
            reply_gate::ROLE.to_string(),
            consumer.clone(),
            serving.clone(),
        ))
    });
    tracing::info!(consumer = %consumer, "interaction daemon: consuming request.resolved per scope");
    super::supervisor::run_per_scope(root, ROLE, move |bus, s| {
        let (db, design, cache, serving, consumer) = (
            db.clone(),
            Arc::clone(&design),
            Arc::clone(&cache),
            serving.clone(),
            consumer.clone(),
        );
        let gate = ReplyHook::of(st.as_ref(), &cache);
        async move {
            let mut me = Interaction::new(db, design, bus)?;
            me.reply_gate = gate;
            let me = Arc::new(me);
            let r = Arc::clone(&me)
                .run(&consumer, ConsumeOptions::default(), cache, serving.clone())
                .await;
            serving.remove(&s);
            r
        }
    })
    .await
}

/// How long the daemon waits for Postgres at boot.
pub const DB_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
