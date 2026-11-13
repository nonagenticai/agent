//! One customer turn (B4 T9): accept a message, then drain it — gate, handover, staff control, qualify, case,
//! `request.qualified` (plan R4/R6/R11, spec 17 N-03).
//!
//! [`accept_message`] is THE send path, channel-agnostic (D109): the staff test chat (`sendChatMessage`), the
//! edge's customer channels and every customer party of the interaction API (crate::runtime::backend) use it; R6's
//! accept-then-answer splits it into [`store_customer_message`] and [`queue_stored_message`]. It:
//!
//! 1. redacts card data ([`redact`], N-03) — the redacted text is the only text stored or sent anywhere;
//! 2. takes the conversation lock (`chat:{conv}:lock`, `SET NX EX 200`): held → 409 `conversation_busy`, nothing
//!    stored (R4);
//! 3. stores the message (`conversations::store::append`, 409 `conversation_ended` for an ended conversation);
//! 4. runs the accept script: the message id onto `chat:{conv}:inbox` and the customer echo on the scope's `chat`
//!    channel, only while the lock is still this turn's;
//! 5. spawns the drain ([`drain::run`]) and returns — the caller answers 202 before the turn runs.
//!
//! The drain releases the lock only when the inbox is empty (one script, so no message is stranded behind a lock
//! about to go away); a drain whose process died leaves the inbox to the [`sweeper`] (every 5 s, per scope), which only
//! re-drains text a customer already sent (no timer starts an LLM call of its own).
//!
//! Redis keys (scoped, R5): `chat:{conv}` (HASH, the qualifier draft, 7 d), `chat:{conv}:inbox` (LIST of message
//! ids, 7 d), `chat:{conv}:lock` (EX 200), `chat:{conv}:redacted` / `chat:{conv}:refused` (EX 3600 throttles); root
//! `turn:refused:{tenant}` (EX 3600, one `channel_turn_refused` audit row per tenant per hour).
//!
//! Chat frames go on the scope's Pub/Sub `chat` channel as `{conversation_id, event: "message", data}` (crate::sse_hub
//! bridges them into the feeds); `data` is the web's `ChatEvent` `{conversation_id, message_id, role, text, case_id?,
//! refused?, at_ms}`. Message text is data: it is never logged.
pub mod drain;
pub mod probe;
pub mod qualify;
pub mod redact;
pub mod sweeper;

use serde_json::{Value, json};
use uuid::Uuid;

use crate::audit::{self, Actor, Event};
use crate::conversations::store::{self, Conv};
use crate::error::{ApiError, ApiResult};
use crate::interaction::Scope as JobScope;
use crate::runtime::{Bus, Env, Scope};
use crate::state::AppState;

/// The conversation lock's lifetime: two qualifier calls (1,200 s) plus slack for Redis and Postgres.
pub const LOCK_SECS: u64 = 1300;
const _: () = assert!(LOCK_SECS > qualify::LLM_TIMEOUT.as_secs() * 2);

/// How long the turn keys live without a write.
pub const TTL_SECS: i64 = 7 * 24 * 3600;

/// One hour: the card-redaction and refusal throttles.
pub const HOUR_SECS: u64 = 3600;

/// The neutral reply of a refused turn (gate 2: licence or tenant not active, K35).
pub const REFUSAL_TEXT: &str = "Sorry, this chat is unavailable right now. Please try again later — we have not \
received this message.";

/// The reply when the turn could not run (the qualifier unbound or unreachable, the bus down).
pub const TURN_FAILED: &str = "Sorry, I could not process your last message: the assistant is unavailable right \
now. Please send it again.";

/// The reply code and text of a TEST-env turn whose `qualify` use cannot be served by the router itself (the model
/// built no client: e.g. a BYO binding that is not callable): the `message` event carries `code`, the stored line the
/// text. Prod keeps [`TURN_FAILED`]. A tenant with no inference-router licence is [`INFERENCE_NOT_PROVISIONED_TEXT`].
pub const QUALIFY_UNBOUND_CODE: &str = "qualify_unbound";
pub const QUALIFY_UNBOUND_TEXT: &str = "This job has no language model connected yet. An administrator can connect one under Integrations \u{2192} Language models.";

/// The TEST-env text (code `llm_inference_not_provisioned`) of a turn whose tenant has no inference-router licence
/// yet; prod answers [`TURN_FAILED`] and logs `turn_not_qualified`.
pub const INFERENCE_NOT_PROVISIONED_TEXT: &str =
    "This tenant\u{2019}s language model access is still being set up.";

/// The scoped key `chat:{conv}{suffix}`.
pub fn chat_key(bus: &Bus, conversation_id: &str, suffix: &str) -> String {
    bus.streams.key(&format!("chat:{conversation_id}{suffix}"))
}

/// 503 `unavailable`: the runtime bus (Redis) is not configured or not reachable.
pub fn unavailable() -> ApiError {
    ApiError::Unavailable(
        "unavailable",
        "The chat runtime is unavailable right now; try again shortly.".into(),
        Some(5),
    )
}

/// A Redis failure as [`unavailable`] (only the error kind is logged).
pub fn redis_failed(e: redis::RedisError) -> ApiError {
    tracing::warn!(kind = ?e.kind(), "turn_redis_failed");
    unavailable()
}

/// 409 `conversation_busy`: a turn of this conversation is running; nothing was stored.
pub fn busy() -> ApiError {
    ApiError::Conflict(
        "conversation_busy",
        "The agent is still answering the last message; send this one when it has replied.".into(),
    )
}

/// The bus scope of a conversation of `s` in `env` (`prod` | `test`).
pub fn runtime_scope(s: &JobScope, env: &str) -> ApiResult<Scope> {
    let env = Env::parse(env).ok_or_else(|| {
        ApiError::Internal(anyhow::anyhow!("conversation env is not prod or test"))
    })?;
    Ok(Scope::new(s.tenant, s.job_key.clone(), env)?)
}

/// Take the conversation lock with `token`; `false` when another turn holds it.
pub async fn acquire(bus: &Bus, conversation_id: &str, token: &str) -> ApiResult<bool> {
    let set: Option<String> = redis::cmd("SET")
        .arg(chat_key(bus, conversation_id, ":lock"))
        .arg(token)
        .arg("NX")
        .arg("EX")
        .arg(LOCK_SECS)
        .query_async(&mut bus.conn())
        .await
        .map_err(redis_failed)?;
    Ok(set.is_some())
}

const RELEASE: &str = "if redis.call('GET', KEYS[1]) == ARGV[1] then redis.call('DEL', KEYS[1]); return 1 end \
return 0";

/// Drop the lock if it is still `token`'s (best effort: it expires on its own).
pub async fn release(bus: &Bus, conversation_id: &str, token: &str) {
    let script = redis::Script::new(RELEASE);
    let mut inv = script.prepare_invoke();
    inv.key(chat_key(bus, conversation_id, ":lock")).arg(token);
    let r: redis::RedisResult<i64> = inv.invoke_async(&mut bus.conn()).await;
    if let Err(e) = r {
        tracing::warn!(kind = ?e.kind(), "turn_lock_release_failed");
    }
}

const ACCEPT: &str = "if redis.call('GET', KEYS[1]) ~= ARGV[1] then return 0 end \
redis.call('RPUSH', KEYS[2], ARGV[2]) \
redis.call('EXPIRE', KEYS[2], ARGV[3]) \
redis.call('HSET', KEYS[4], ARGV[2], 'q') \
redis.call('EXPIRE', KEYS[4], ARGV[3]) \
redis.call('EXPIRE', KEYS[1], ARGV[4]) \
redis.call('PUBLISH', KEYS[3], ARGV[5]) \
return 1";

/// Queue `message_id` for the drain (its [`MessageState`] `queued`) and publish the customer echo, only while
/// `token` holds the lock.
async fn accept_script(
    bus: &Bus,
    conversation_id: &str,
    token: &str,
    message_id: &str,
    frame: &Value,
) -> ApiResult<bool> {
    let script = redis::Script::new(ACCEPT);
    let mut inv = script.prepare_invoke();
    inv.key(chat_key(bus, conversation_id, ":lock"))
        .key(chat_key(bus, conversation_id, ":inbox"))
        .key(bus.streams.chat())
        .key(chat_key(bus, conversation_id, ":msgs"))
        .arg(token)
        .arg(message_id)
        .arg(TTL_SECS)
        .arg(LOCK_SECS)
        .arg(frame.to_string());
    let n: i64 = inv
        .invoke_async(&mut bus.conn())
        .await
        .map_err(redis_failed)?;
    Ok(n == 1)
}

const TAKE: &str = "local v = redis.call('LRANGE', KEYS[1], 0, -1); redis.call('DEL', KEYS[1]) \
for _, id in ipairs(v) do redis.call('HSET', KEYS[2], id, ARGV[1]) end \
if #v > 0 then redis.call('EXPIRE', KEYS[2], ARGV[2]) end \
return v";

/// Every queued message id (oldest first); the inbox is emptied and each id is marked taken now
/// ([`MessageState::Taken`]: one turn = the ids taken together, its reply stored after that instant).
pub async fn take_inbox(bus: &Bus, conversation_id: &str) -> ApiResult<Vec<String>> {
    let script = redis::Script::new(TAKE);
    let mut inv = script.prepare_invoke();
    inv.key(chat_key(bus, conversation_id, ":inbox"))
        .key(chat_key(bus, conversation_id, ":msgs"))
        .arg(crate::conversations::now_ms())
        .arg(TTL_SECS);
    inv.invoke_async(&mut bus.conn())
        .await
        .map_err(redis_failed)
}

/// Where a stored customer message is on its way to a turn (`chat:{conv}:msgs`, field = message id, 7 d).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MessageState {
    /// Nothing recorded: no runtime bus, an expired hash, or a message the runtime never saw (B12).
    Unknown,
    /// Stored by [`store_customer_message`] (R6's accept): no turn yet.
    Stored,
    /// On the inbox, not yet taken by a drain.
    Queued,
    /// Taken by a drain at this instant (ms): its turn's reply is the first agent line stored after it.
    Taken(i64),
}

impl MessageState {
    fn parse(v: Option<&str>) -> Self {
        match v {
            None => MessageState::Unknown,
            Some("s") => MessageState::Stored,
            Some("q") => MessageState::Queued,
            Some(ms) => ms
                .parse()
                .map_or(MessageState::Unknown, MessageState::Taken),
        }
    }
}

/// The scoped bus of `conv` (`None` without `REDIS_URL` or for an env that is not prod/test).
fn conv_bus(st: &AppState, scope: &JobScope, conv: &Conv) -> Option<Bus> {
    let root = st.redis.as_ref()?;
    let rs = runtime_scope(scope, &conv.env).ok()?;
    Some(root.scoped(&rs))
}

/// The [`MessageState`] of customer message `message_id` of `conv` (`Unknown` when Redis cannot say).
pub async fn message_state(
    st: &AppState,
    scope: &JobScope,
    conv: &Conv,
    message_id: Uuid,
) -> MessageState {
    let Some(bus) = conv_bus(st, scope, conv) else {
        return MessageState::Unknown;
    };
    let v: redis::RedisResult<Option<String>> = redis::cmd("HGET")
        .arg(chat_key(&bus, &conv.id, ":msgs"))
        .arg(message_id.to_string())
        .query_async(&mut bus.conn())
        .await;
    match v {
        Ok(v) => MessageState::parse(v.as_deref()),
        Err(e) => {
            tracing::warn!(kind = ?e.kind(), "turn_message_state_unreadable");
            MessageState::Unknown
        }
    }
}

/// The ids a drain took together with another at `taken_ms` (one turn's batch), `Taken(taken_ms)` included.
pub async fn taken_together(
    st: &AppState,
    scope: &JobScope,
    conv: &Conv,
    taken_ms: i64,
) -> ApiResult<Vec<Uuid>> {
    let Some(bus) = conv_bus(st, scope, conv) else {
        return Ok(Vec::new());
    };
    let all: std::collections::HashMap<String, String> = redis::cmd("HGETALL")
        .arg(chat_key(&bus, &conv.id, ":msgs"))
        .query_async(&mut bus.conn())
        .await
        .map_err(redis_failed)?;
    let ms = taken_ms.to_string();
    Ok(all
        .into_iter()
        .filter(|(_, v)| *v == ms)
        .filter_map(|(k, _)| Uuid::parse_str(&k).ok())
        .collect())
}

/// What [`finish`] did with the lock.
#[derive(Debug, PartialEq, Eq)]
pub enum Finish {
    /// The inbox was empty: the lock is released.
    Released,
    /// Ids arrived meanwhile: the lock is kept (TTL refreshed) and the holder drains again.
    Pending,
    /// The lock is no longer this turn's; nothing changed.
    Lost,
}

const FINISH: &str = "if redis.call('GET', KEYS[1]) ~= ARGV[1] then return -1 end \
if redis.call('LLEN', KEYS[2]) > 0 then redis.call('EXPIRE', KEYS[1], ARGV[2]); return 1 end \
redis.call('DEL', KEYS[1]); return 0";

/// End one drain: release the lock only when the inbox is empty, in one script.
pub async fn finish(bus: &Bus, conversation_id: &str, token: &str) -> ApiResult<Finish> {
    let script = redis::Script::new(FINISH);
    let mut inv = script.prepare_invoke();
    inv.key(chat_key(bus, conversation_id, ":lock"))
        .key(chat_key(bus, conversation_id, ":inbox"))
        .arg(token)
        .arg(LOCK_SECS);
    let n: i64 = inv
        .invoke_async(&mut bus.conn())
        .await
        .map_err(redis_failed)?;
    Ok(match n {
        1 => Finish::Pending,
        0 => Finish::Released,
        _ => Finish::Lost,
    })
}

/// `SET key 1 NX EX secs`: `true` the first time within `secs`.
pub async fn first_within(bus: &Bus, key: &str, secs: u64) -> ApiResult<bool> {
    let set: Option<String> = redis::cmd("SET")
        .arg(key)
        .arg("1")
        .arg("NX")
        .arg("EX")
        .arg(secs)
        .query_async(&mut bus.conn())
        .await
        .map_err(redis_failed)?;
    Ok(set.is_some())
}

/// The scope's `chat` channel frame of one message (crate::sse_hub bridges it into the feeds as event `message`).
pub fn message_frame(conversation_id: &str, data: Value) -> Value {
    json!({"conversation_id": conversation_id, "event": "message", "data": data})
}

/// PUBLISH one message on the scope's `chat` channel (lossy by design: the message is already stored).
pub async fn publish_message(bus: &Bus, conversation_id: &str, data: &Value) {
    if let Err(e) = crate::sse_hub::publish_chat(bus, conversation_id, "message", data).await {
        tracing::warn!(error = %format!("{e:#}"), "turn_chat_publish_failed");
    }
}

/// One customer message to accept.
pub struct Inbound<'a> {
    pub scope: &'a JobScope,
    pub conv: &'a Conv,
    /// The message's `author` (the customer's name or id).
    pub author: &'a str,
    /// Who the turn's audit rows name (the staff member of a test chat, the edge service for a customer channel).
    pub actor: Actor,
    pub text: &'a str,
    /// The provider's message id (a retry is refused 409 `duplicate_message`); `None` for the staff chat.
    pub external_id: Option<&'a str>,
}

/// Who wrote an accepted message: the conversation's end user (stored role `customer`) or a machine caller of the
/// job (an API key; stored role `caller`, migration 0234).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthorRole {
    User,
    Caller,
}

impl AuthorRole {
    /// The `conversation_message.role` the message is stored with.
    pub fn stored(self) -> &'static str {
        match self {
            AuthorRole::User => "customer",
            AuthorRole::Caller => "caller",
        }
    }
}

/// An accepted message: stored, queued, its echo published; its turn runs after this returns.
#[derive(Clone, Debug)]
pub struct Accepted {
    pub message_id: Uuid,
    pub at_ms: i64,
    /// Card data was removed from it (N-03).
    pub redacted: bool,
}

/// Accept one message of `author_role` (see the module doc): 409 `conversation_busy` (nothing stored) while a turn
/// runs, 409 `conversation_ended`, 503 `unavailable` without the runtime bus.
pub async fn accept_message(
    st: &AppState,
    m: Inbound<'_>,
    author_role: AuthorRole,
) -> ApiResult<Accepted> {
    let root = st.redis.as_ref().ok_or_else(unavailable)?;
    let rs = runtime_scope(m.scope, &m.conv.env)?;
    let bus = root.scoped(&rs);
    let red = redact::redact(m.text);
    let token = Uuid::new_v4().to_string();
    if !acquire(&bus, &m.conv.id, &token).await? {
        return Err(busy());
    }
    // A2: the answer to a parked secret question is sealed as state here and stored as `[redacted]`
    // (a machine caller's answer is sealed too: it answers the same parked question, and its role is no reason to
    // send the task field empty)
    let red = drain::seal_secret_answer(
        st,
        &bus,
        m.scope.tenant,
        m.scope.job_id,
        &m.conv.id,
        m.text,
        red,
    )
    .await;
    let stored = match store_message(st, &bus, &m, &red, author_role).await {
        Ok(s) => s,
        Err(e) => {
            release(&bus, &m.conv.id, &token).await;
            return Err(e);
        }
    };
    let message_id = stored.message_id.to_string();
    let frame = message_frame(
        &m.conv.id,
        json!({
            "conversation_id": m.conv.id, "message_id": message_id, "role": author_role.stored(), "author": m.author,
            "text": red.text, "at_ms": stored.at_ms,
        }),
    );
    match accept_script(&bus, &m.conv.id, &token, &message_id, &frame).await {
        Ok(true) => {}
        Ok(false) => {
            // the lock outlived LOCK_SECS between two calls: no turn for this message, the caller may resend
            tracing::warn!(conversation = %m.conv.id, "turn_lock_lost_at_accept");
            return Err(unavailable());
        }
        Err(e) => {
            release(&bus, &m.conv.id, &token).await;
            return Err(e);
        }
    }
    let ctx = drain::TurnCtx {
        tenant: m.scope.tenant,
        job_id: m.scope.job_id,
        job_key: m.scope.job_key.clone(),
        env: rs.env,
        conversation_id: m.conv.id.clone(),
    };
    tokio::spawn(drain::run(st.clone(), ctx, token));
    Ok(Accepted {
        message_id: stored.message_id,
        at_ms: stored.at_ms,
        redacted: red.changed,
    })
}

const MARK_STORED: &str =
    "redis.call('HSET', KEYS[1], ARGV[1], 's'); redis.call('EXPIRE', KEYS[1], ARGV[2]); return 1";

/// R6 persist-before-200 on the runtime (D109): store one customer message (card data redacted, as
/// [`accept_message`] does) WITHOUT starting its turn; the edge's second call runs it with
/// [`queue_stored_message`]. 409 `conversation_ended`, `duplicate_message`; 503 `unavailable` without the runtime bus.
pub async fn store_customer_message(st: &AppState, m: Inbound<'_>) -> ApiResult<Accepted> {
    let root = st.redis.as_ref().ok_or_else(unavailable)?;
    let rs = runtime_scope(m.scope, &m.conv.env)?;
    let bus = root.scoped(&rs);
    let red = redact::redact(m.text);
    let red = drain::seal_secret_answer(
        st,
        &bus,
        m.scope.tenant,
        m.scope.job_id,
        &m.conv.id,
        m.text,
        red,
    )
    .await;
    let stored = store_message(st, &bus, &m, &red, AuthorRole::User).await?;
    // best effort: without the mark the edge's second call falls back to the stored-history rule
    let script = redis::Script::new(MARK_STORED);
    let mut inv = script.prepare_invoke();
    inv.key(chat_key(&bus, &m.conv.id, ":msgs"))
        .arg(stored.message_id.to_string())
        .arg(TTL_SECS);
    let marked: redis::RedisResult<i64> = inv.invoke_async(&mut bus.conn()).await;
    if let Err(e) = marked {
        tracing::warn!(kind = ?e.kind(), "turn_stored_mark_failed");
    }
    Ok(Accepted {
        message_id: stored.message_id,
        at_ms: stored.at_ms,
        redacted: red.changed,
    })
}

const ENQUEUE: &str = "local s = redis.call('HGET', KEYS[2], ARGV[1]) \
if s and s ~= 's' then return 0 end \
redis.call('HSET', KEYS[2], ARGV[1], 'q') \
redis.call('EXPIRE', KEYS[2], ARGV[2]) \
redis.call('RPUSH', KEYS[1], ARGV[1]) \
redis.call('EXPIRE', KEYS[1], ARGV[2]) \
return 1";

/// Queue customer message `message_id` of `conv`, already stored ([`store_customer_message`], or a message whose
/// turn never started), for a turn — never refused for a busy conversation: the id goes onto the inbox (`queued`),
/// then the drain starts if the lock is free; a turn still running takes it when it finishes (`finish` sees the
/// inbox), and a lock that died meanwhile leaves it to the [`sweeper`]. Wait for its reply with
/// [`MessageState::Taken`]. The stored → queued step is one check-and-set: a message is queued at most once, and
/// `false` says another call queued it already (its turn's reply goes out on that call). 404 `not_found` for an id
/// that is not a customer message of `conv`, 503 `unavailable` without the bus.
pub async fn queue_stored_message(
    st: &AppState,
    scope: &JobScope,
    conv: &Conv,
    message_id: Uuid,
) -> ApiResult<bool> {
    let root = st.redis.as_ref().ok_or_else(unavailable)?;
    let rs = runtime_scope(scope, &conv.env)?;
    let bus = root.scoped(&rs);
    let Some((at_ms, author, text)) = stored_line(st, scope, &conv.id, message_id).await? else {
        return Err(ApiError::Missing(
            "not_found",
            "No such customer message in this conversation.".into(),
        ));
    };
    let id = message_id.to_string();
    let script = redis::Script::new(ENQUEUE);
    let mut inv = script.prepare_invoke();
    inv.key(chat_key(&bus, &conv.id, ":inbox"))
        .key(chat_key(&bus, &conv.id, ":msgs"))
        .arg(&id)
        .arg(TTL_SECS);
    let queued: i64 = inv
        .invoke_async(&mut bus.conn())
        .await
        .map_err(redis_failed)?;
    if queued == 0 {
        return Ok(false);
    }
    let echo = json!({
        "conversation_id": conv.id, "message_id": id, "role": "customer", "author": author,
        "text": text, "at_ms": at_ms,
    });
    publish_message(&bus, &conv.id, &echo).await;
    let token = Uuid::new_v4().to_string();
    if acquire(&bus, &conv.id, &token).await? {
        let ctx = drain::TurnCtx {
            tenant: scope.tenant,
            job_id: scope.job_id,
            job_key: scope.job_key.clone(),
            env: rs.env,
            conversation_id: conv.id.clone(),
        };
        tokio::spawn(drain::run(st.clone(), ctx, token));
    }
    Ok(true)
}

/// `(at_ms, author, text)` of customer message `message_id` of `conversation_id`.
async fn stored_line(
    st: &AppState,
    scope: &JobScope,
    conversation_id: &str,
    message_id: Uuid,
) -> ApiResult<Option<(i64, String, String)>> {
    let mut tx = store::begin_job(&st.db, scope.tenant, scope.job_id).await?;
    let row: Option<(i64, String, String)> = sqlx::query_as(
        "SELECT at_ms, COALESCE(author, ''), text FROM conversation_message \
          WHERE message_id = $1 AND conversation_id = $2 AND role IN ('customer', 'caller')",
    )
    .bind(message_id)
    .bind(conversation_id)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(row)
}

/// Store the (redacted) message in one transaction; audit a redaction once per conversation per hour.
async fn store_message(
    st: &AppState,
    bus: &Bus,
    m: &Inbound<'_>,
    red: &redact::Redacted,
    author_role: AuthorRole,
) -> ApiResult<store::Stored> {
    let mut tx = store::begin_job(&st.db, m.scope.tenant, m.scope.job_id).await?;
    if red.changed {
        // the audit row's FK takes the tenants row: lock it first (one lock order)
        crate::tenancy::lock_tenant_row(&mut tx, m.scope.tenant).await?;
    }
    let ended: Option<bool> = sqlx::query_scalar(
        "SELECT ended_at IS NOT NULL FROM conversation WHERE conversation_id = $1",
    )
    .bind(&m.conv.id)
    .fetch_optional(&mut *tx)
    .await?;
    match ended {
        None => {
            return Err(ApiError::Missing(
                "conversation_not_found",
                "No such conversation in this job.".into(),
            ));
        }
        Some(true) => {
            return Err(ApiError::Conflict(
                "conversation_ended",
                "This conversation has ended. Start a new one.".into(),
            ));
        }
        Some(false) => {}
    }
    let stored = store::append(
        &mut tx,
        store::NewMessage {
            job_id: m.scope.job_id,
            conversation_id: &m.conv.id,
            role: author_role.stored(),
            author: m.author,
            text: &red.text,
            case_id: None,
            external_id: m.external_id,
        },
    )
    .await?
    .ok_or_else(|| {
        ApiError::Conflict(
            "duplicate_message",
            "This message was already received.".into(),
        )
    })?;
    if red.changed && first_within(bus, &chat_key(bus, &m.conv.id, ":redacted"), HOUR_SECS).await? {
        audit::record(
            &mut tx,
            &m.actor,
            Event::new("chat.card_redacted", "conversation", m.conv.id.clone())
                .job(&m.scope.job_key)
                .detail("message_id", stored.message_id.to_string())
                .detail("channel", m.conv.channel.clone()),
        )
        .await?;
    }
    tx.commit().await?;
    Ok(stored)
}

/// Who audits a refused conversation start.
const START_SERVICE: &str = "service:agent-api-turn";

/// 409 `tenant_not_active` with the customer's neutral [`REFUSAL_TEXT`] (the internal door's code for a tenant that
/// is not active, crate::conversations::internal).
pub fn tenant_not_active() -> ApiError {
    ApiError::Conflict("tenant_not_active", REFUSAL_TEXT.into())
}

/// Gate 2 at a conversation's start (live acceptance A10): may `tenant` open a new conversation? The same
/// [`drain::TenantStatusGate`] the drain asks before every turn, so a suspended (or archived) tenant can neither
/// start a chat nor have a turn answered. Refused → 409 [`tenant_not_active`]; the caller stores nothing. A refusal
/// is audited `channel_turn_refused` (`detail.stage = "start"`) under the drain's own throttle key
/// `turn:refused:{tenant}`, so a tenant still gets at most one such row per hour, start or turn. Without the
/// runtime bus there is no throttle, and nothing is audited.
pub async fn admit_start(
    st: &AppState,
    tenant: Uuid,
    job_key: &str,
    channel: &str,
) -> ApiResult<()> {
    use drain::LicenceGate as _;
    if drain::TenantStatusGate.admits(st, tenant).await? {
        return Ok(());
    }
    if let Some(root) = st.redis.as_ref() {
        let key = root.root_key(&format!("turn:refused:{}", tenant.hyphenated()));
        if first_within(root, &key, HOUR_SECS).await? {
            let mut tx = crate::tenancy::begin_scoped(&st.db, tenant).await?;
            crate::tenancy::lock_tenant_row(&mut tx, tenant).await?;
            // no conversation exists to name: the subject is the tenant, the job its detail
            let mut event = Event::new("channel_turn_refused", "tenant", tenant.to_string())
                .detail("code", "tenant_not_active")
                .detail("stage", "start")
                .detail("channel", channel.to_string());
            if !job_key.trim().is_empty() {
                event = event.job(job_key);
            }
            audit::record(&mut tx, &Actor::system(START_SERVICE), event).await?;
            tx.commit().await?;
        }
    }
    Err(tenant_not_active())
}
