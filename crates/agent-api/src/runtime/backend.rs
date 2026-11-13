//! Customer channels on the runtime (B4 T9c, §0 R7 + R3): [`RuntimeBackend`], the B4 runtime as an
//! [`InteractionBackend`].
//!
//! A customer message the agent would answer (crate::conversations::internal `inbound`: no open handover item, no
//! staff control, no request for a person; R6's `accept_only` stores it and the edge's second call runs it) is a
//! runtime turn: [`RuntimeBackend::turn`] hands
//! it to `turn::accept_message` (card data redacted, stored, queued, drained: gate, qualify, case,
//! `request.qualified` → orchestrator → runner → interaction) and waits until the runtime has stored the agent's line
//! after it ([`RuntimeBackend::reply_after`]). So every channel gets cases, operation calls and handovers, and its
//! replies are grounded by the interaction daemon in the reply's own transaction (§0 R3) — the caller stores
//! nothing for a runtime reply.
//!
//! The wait reads the stored reply, not the `chat` frame: frames are lossy by design (the message is stored first),
//! a row is not. It ends at the first agent line after the customer's message (with the CSAT prompt the daemon
//! stored in the same transaction appended), a handover item opened for the conversation, or staff control; after
//! [`REPLY_WAIT`] it is 503 `unavailable` (the reply, when it comes, is in the conversation's history).
//!
//! D109 (KH T6): this is the ONE answer path of a customer party. [`RuntimeBackend::serving`] says whether a live
//! `interaction` process serves the scope (its heartbeat's `serving` map, crate::runtime::heartbeat; `REDIS_URL`
//! set); a heartbeat read that fails counts as not served. A customer message for a job with no live release is 409
//! `job_not_live` ([`job_live`]); one for a live job whose scope is not served is 503 `unavailable` — never a generic
//! model reply (`AppState.interaction`, the platform passthrough, answers only the staff playground).
//! [`runtime_stream`] is the interaction API's runtime path (API clients, MCP, voice, email relay, async webhooks:
//! crate::interaction::respond): it creates the request's conversation first ([`conversation_channel`]: `api`, or
//! `email` / `voice` from those connectors' own `ConversationRef`; the ref = that id, else the request id, so a
//! stateless request is its own conversation, opened with its earlier turns, and an async retry finds its message
//! again; a caller's own `conversation.id` is a thread key, scoped by job, and an API key caller (role `system`, stored
//! message role `system`) writes its key and label as the conversation's `caller` state, A4).
//! "Live" = a live release whose runtime design compiles ([`job_live`]). R6's accept-then-answer follows each
//! message's `turn::MessageState`: the answer call queues it behind a running turn ([`RuntimeBackend::stored_turn`])
//! and waits for the reply of the turn that took it ([`RuntimeBackend::taken_reply`]).
//!
//! Customer text is data: only ids and codes are logged.
use std::time::Duration;

use futures_util::StreamExt;
use futures_util::stream::{self, BoxStream};
use sqlx::PgConnection;
use uuid::Uuid;

use super::interaction::ROLE as INTERACTION_ROLE;
use crate::audit::Actor;
use crate::conversations::{handover, ownership, state as conv_state, store};
use crate::error::{ApiError, ApiResult};
use crate::interaction::{
    BackendError, BackendEvent, ConversationRef, InteractionBackend, InteractionRequest, Message,
    PartyRole, Role, Scope as JobScope,
};
use crate::state::AppState;
use crate::turn::{self, AuthorRole, Inbound};

/// How long a channel request waits for the runtime's reply: under the edge's own bound on `inbound`.
pub const REPLY_WAIT: Duration = Duration::from_secs(100);
const _: () = assert!(REPLY_WAIT.as_secs() < crate::channels::core::INBOUND_TIMEOUT.as_secs());

/// How often the wait re-reads the conversation.
const POLL: Duration = Duration::from_millis(200);

/// How long a new message waits for the previous turn's lock before it is 409 `conversation_busy`.
const BUSY_WAIT: Duration = Duration::from_secs(2);

/// The audit actor of a turn an in-process connector starts through [`InteractionBackend::respond`].
const SERVICE: &str = "service:agent-api-runtime";

/// How a runtime turn ended for the customer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The agent's line is stored (and, for a case reply, grounded): `text` is what the customer is sent.
    Replied {
        message_id: Uuid,
        case_id: Option<String>,
        text: String,
    },
    /// The turn queued a handover item (a request gate's handover): a person answers.
    Queued { case_id: String, item_id: String },
    /// Staff took the conversation over while the turn ran: the agent's reply is withheld.
    StaffControl,
}

/// The B4 runtime as the answering backend of a customer channel (module doc).
#[derive(Clone)]
pub struct RuntimeBackend {
    st: AppState,
}

/// One agent line after the customer's message.
#[derive(sqlx::FromRow)]
struct Line {
    message_id: Uuid,
    text: String,
    case_id: Option<String>,
}

impl RuntimeBackend {
    /// The runtime for conversations of `scope` in `env` (`prod` | `test`), when a live `interaction` process serves
    /// that scope; `None` (the caller answers 503 `unavailable`) without `REDIS_URL`, without such a process, or
    /// when the heartbeats cannot be read.
    pub async fn serving(st: &AppState, scope: &JobScope, env: &str) -> Option<RuntimeBackend> {
        let root = st.redis.as_ref()?;
        let rs = turn::runtime_scope(scope, env).ok()?;
        let key = rs.to_string();
        match crate::bus_admin::beats(root, INTERACTION_ROLE).await {
            Ok(beats) if beats.iter().any(|b| b.serving.contains_key(&key)) => {
                Some(RuntimeBackend { st: st.clone() })
            }
            Ok(_) => None,
            Err(e) => {
                tracing::warn!(kind = ?e.kind(), "runtime_backend_heartbeats_unreadable");
                None
            }
        }
    }

    /// Run end-user message `m` as a runtime turn and wait for its outcome: 409 `conversation_busy` (nothing stored)
    /// while another turn of the conversation runs, 409 `conversation_ended`, 503 `unavailable` (no reply within
    /// [`REPLY_WAIT`], or Redis down).
    pub async fn turn(&self, m: Inbound<'_>) -> ApiResult<Outcome> {
        self.turn_as(m, AuthorRole::User).await
    }

    /// [`RuntimeBackend::turn`] for a message of `author_role` (a machine caller's is stored `system`).
    pub async fn turn_as(&self, m: Inbound<'_>, author_role: AuthorRole) -> ApiResult<Outcome> {
        let scope = m.scope.clone();
        let conversation_id = m.conv.id.clone();
        let accepted = self.accept(m, author_role).await?;
        self.reply_after(
            &scope,
            &conversation_id,
            accepted.at_ms,
            accepted.message_id,
        )
        .await
    }

    /// `turn::accept_message`, retried for up to [`BUSY_WAIT`] while the lock is held: the previous turn's
    /// drain lets go of it right after storing its reply, so a message sent on that reply is not refused 409
    /// `conversation_busy` for a lock about to be released. A turn still running after that is 409 (nothing stored).
    async fn accept(&self, m: Inbound<'_>, author_role: AuthorRole) -> ApiResult<turn::Accepted> {
        let until = tokio::time::Instant::now() + BUSY_WAIT;
        loop {
            let again = Inbound {
                scope: m.scope,
                conv: m.conv,
                author: m.author,
                actor: m.actor.clone(),
                text: m.text,
                external_id: m.external_id,
            };
            match turn::accept_message(&self.st, again, author_role).await {
                Err(e)
                    if e.code() == "conversation_busy" && tokio::time::Instant::now() < until =>
                {
                    tokio::time::sleep(POLL).await;
                }
                r => return r,
            }
        }
    }

    /// Wait for the outcome of the turn of customer message `message_id` (stored at `at_ms`) of `conversation_id`.
    pub async fn reply_after(
        &self,
        scope: &JobScope,
        conversation_id: &str,
        at_ms: i64,
        message_id: Uuid,
    ) -> ApiResult<Outcome> {
        let until = tokio::time::Instant::now() + REPLY_WAIT;
        loop {
            if let Some(o) = self
                .outcome(scope, conversation_id, (at_ms, message_id), message_id)
                .await?
            {
                return Ok(o);
            }
            if tokio::time::Instant::now() >= until {
                tracing::warn!(conversation = %conversation_id, "runtime_reply_timeout");
                return Err(turn::unavailable());
            }
            tokio::time::sleep(POLL).await;
        }
    }

    /// The outcome so far: the first agent line after `after` (`(at_ms, message_id)`), else a handover item, else
    /// staff control. `message_id` is the customer message whose case the reply may name.
    async fn outcome(
        &self,
        scope: &JobScope,
        conversation_id: &str,
        after: (i64, Uuid),
        message_id: Uuid,
    ) -> ApiResult<Option<Outcome>> {
        let (at_ms, after_id) = after;
        let mut tx = store::begin_job(&self.st.db, scope.tenant, scope.job_id).await?;
        let lines: Vec<Line> = sqlx::query_as(
            "SELECT message_id, text, case_id FROM conversation_message \
              WHERE conversation_id = $1 AND role = 'agent' AND author IS DISTINCT FROM $4 AND (at_ms, message_id) > ($2, $3) \
              ORDER BY at_ms, message_id LIMIT 1",
        )
        .bind(conversation_id)
        .bind(at_ms)
        .bind(after_id)
        .bind(crate::runtime::task_relay::AUTHOR)
        .fetch_all(&mut *tx)
        .await?;
        let out = if let Some(first) = lines.first() {
            let linked: Option<Option<String>> = sqlx::query_scalar(
                "SELECT case_id FROM conversation_message WHERE message_id = $1",
            )
            .bind(message_id)
            .fetch_optional(&mut *tx)
            .await?;
            // SW-6: the agent never stores a CSAT prompt with its reply (it arms the request; the prompt follows the
            // customer's acknowledgement), so the reply is the first line alone
            Some(Outcome::Replied {
                message_id: first.message_id,
                case_id: first.case_id.clone().or(linked.flatten()),
                text: first.text.clone(),
            })
        } else if ownership::silenced(&ownership::read(&mut tx, conversation_id).await?.owner) {
            // SW-1: anyone but the agent owns the conversation; an open item says who answers
            match handover::open_for_conversation(&mut tx, conversation_id).await? {
                Some(item) => Some(Outcome::Queued {
                    case_id: item.case_id,
                    item_id: item.id,
                }),
                None => Some(Outcome::StaffControl),
            }
        } else {
            None
        };
        tx.commit().await?;
        Ok(out)
    }

    /// Queue the stored customer message `message_id` of `conv` (R6's accepted message, or one whose turn never
    /// started; never refused for a busy conversation: `turn::queue_stored_message`) and wait for its turn's outcome
    /// ([`RuntimeBackend::taken_reply`]). `None` when another call queued it first (it delivers the reply), or when a
    /// later message of the same turn does.
    pub async fn stored_turn(
        &self,
        scope: &JobScope,
        conv: &store::Conv,
        message_id: Uuid,
    ) -> ApiResult<Option<Outcome>> {
        if !turn::queue_stored_message(&self.st, scope, conv, message_id).await? {
            // another call queued it: that call delivers its turn's reply
            return Ok(None);
        }
        self.taken_reply(scope, conv, message_id).await
    }

    /// Wait until a drain took queued message `message_id` of `conv`, then for the outcome of that turn: the first
    /// agent line stored after the take (a reply of an earlier turn that landed after this message was stored is
    /// never taken for this one). `None` when a later customer message was taken in the same turn: that message's
    /// call delivers the turn's one reply. 503 `unavailable` after [`REPLY_WAIT`].
    pub async fn taken_reply(
        &self,
        scope: &JobScope,
        conv: &store::Conv,
        message_id: Uuid,
    ) -> ApiResult<Option<Outcome>> {
        let until = tokio::time::Instant::now() + REPLY_WAIT;
        loop {
            if let turn::MessageState::Taken(ms) =
                turn::message_state(&self.st, scope, conv, message_id).await
                && let Some(o) = self.after_take(scope, conv, ms, message_id).await?
            {
                if self.later_in_turn(scope, conv, ms, message_id).await? {
                    return Ok(None);
                }
                return Ok(Some(o));
            }
            if tokio::time::Instant::now() >= until {
                tracing::warn!(conversation = %conv.id, "runtime_reply_timeout");
                return Err(turn::unavailable());
            }
            tokio::time::sleep(POLL).await;
        }
    }

    /// The outcome of the turn that took message `message_id` at `taken_ms` (lines strictly after that instant).
    pub async fn after_take(
        &self,
        scope: &JobScope,
        conv: &store::Conv,
        taken_ms: i64,
        message_id: Uuid,
    ) -> ApiResult<Option<Outcome>> {
        self.outcome(
            scope,
            &conv.id,
            (taken_ms, Uuid::from_u128(u128::MAX)),
            message_id,
        )
        .await
    }

    /// Was a customer message stored after `message_id` taken in the same turn (at `taken_ms`)?
    async fn later_in_turn(
        &self,
        scope: &JobScope,
        conv: &store::Conv,
        taken_ms: i64,
        message_id: Uuid,
    ) -> ApiResult<bool> {
        let mates = turn::taken_together(&self.st, scope, conv, taken_ms).await?;
        if mates.len() < 2 {
            return Ok(false);
        }
        let mut tx = store::begin_job(&self.st.db, scope.tenant, scope.job_id).await?;
        let later: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM conversation_message m, conversation_message me \
                             WHERE me.message_id = $1 AND m.conversation_id = me.conversation_id \
                               AND m.message_id = ANY($2) AND m.role IN ('customer', 'caller') \
                               AND (m.at_ms, m.message_id) > (me.at_ms, me.message_id))",
        )
        .bind(message_id)
        .bind(&mates)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(later)
    }
}

/// The env of every customer conversation (the API-key interaction path; a channel binding's env is its own, D130).
pub const CUSTOMER_ENV: &str = "prod";

/// 409 `job_not_live` (D109): the job has no live release, so no customer is answered.
pub fn job_not_live() -> ApiError {
    ApiError::Conflict(
        "job_not_live",
        "This job has no live release yet; customers reach it once a release is live.".into(),
    )
}

/// D109: the job is live — it has a live release (`jobs.current_release_id`) AND that release's runtime design loads
/// with no template error (the Live design the turn runs on: its pinned template stack compiles with the release's
/// selection; `RuntimeDesign::template_errors` empty) — else 409 `job_not_live`, naming the first template error
/// for operators. The design comes from `AppState.designs`, the cache the turn's drain reads too. `conn` is a
/// [`store::begin_job`] transaction of the job's tenant.
pub async fn job_live(st: &AppState, conn: &mut PgConnection, scope: &JobScope) -> ApiResult<()> {
    let live: Option<Option<String>> =
        sqlx::query_scalar("SELECT current_release_id FROM jobs WHERE id = $1")
            .bind(scope.job_id)
            .fetch_optional(&mut *conn)
            .await?;
    if live.flatten().is_none() {
        return Err(job_not_live());
    }
    let rs = super::Scope::new(scope.tenant, scope.job_key.clone(), super::Env::Prod)?;
    let job = crate::design::JobRef {
        id: scope.job_id,
        key: scope.job_key.clone(),
    };
    let design = st.designs.get_or_load(conn, &st.design, &rs, &job).await?;
    match design.template_errors.first() {
        None => Ok(()),
        Some(first) => {
            tracing::warn!(job = %scope.job_key, error = %first, "customer_job_without_runtime_design");
            Err(ApiError::Conflict(
                "job_not_live",
                format!(
                    "This job's live release has no runtime design ({first}); customers reach it once a release \
                     that compiles is live."
                ),
            ))
        }
    }
}

/// The in-process connectors that name their own conversation channel (their handlers set the request's
/// `ConversationRef`; no API shape can).
pub const CONNECTOR_CHANNELS: [&str; 1] = ["voice"];

/// The conversation channel of a customer request: the channel of the `ConversationRef` an in-process connector set
/// ([`CONNECTOR_CHANNELS`]), else `api` — an API caller's `channel` / `X-Interaction-Channel` labels the request,
/// never the conversation (it cannot pass itself off as Telegram or web chat).
pub fn conversation_channel(req: &InteractionRequest) -> &'static str {
    req.conversation
        .as_ref()
        .and_then(|c| CONNECTOR_CHANNELS.iter().copied().find(|k| *k == c.channel))
        .unwrap_or("api")
}

/// The conversation a Responses `previous_response_id` continues (B4, B10 §3): `previous` is the request id of an
/// earlier turn; the answer is the API conversation's customer ref, so [`runtime_turn`] joins it. Only a turn of the
/// same tenant and job (the scoped transaction and the explicit filters), on an `api` conversation of the same customer
/// party, resolves; anything else is 404 `response_not_found` — never another tenant's, job's or party's conversation,
/// and no way to tell which. A request without a customer party (the staff playground) stores nothing to continue.
pub async fn previous_conversation(
    st: &AppState,
    req: &InteractionRequest,
    previous: Uuid,
) -> ApiResult<ConversationRef> {
    let Some(party) = req
        .party
        .as_ref()
        .filter(|_| crate::interaction::is_runtime_caller(req))
    else {
        return Err(ApiError::BadRequest(
            "unsupported_parameter",
            "previous_response_id continues a customer conversation; the staff playground stores none".into(),
        ));
    };
    let mut tx = store::begin_job(&st.db, req.tenant, req.job_id).await?;
    let found: Option<String> = sqlx::query_scalar(
        "SELECT r.external_id FROM conversation_message m \
           JOIN conversation c ON c.tenant_id = m.tenant_id AND c.job_id = m.job_id \
                              AND c.conversation_id = m.conversation_id \
           JOIN channel_conversation_refs r ON r.tenant_id = c.tenant_id AND r.job_id = c.job_id \
                              AND r.conversation_id = c.conversation_id AND r.side = 'customer' AND r.channel = 'api' \
          WHERE m.tenant_id = $1 AND m.job_id = $2 AND m.external_id = $3 AND m.role IN ('customer', 'caller') \
            AND c.channel = 'api' AND c.customer_id = $4 \
          LIMIT 1",
    )
    .bind(req.tenant)
    .bind(req.job_id)
    .bind(previous.to_string())
    .bind(&party.id)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    let id = found.ok_or_else(crate::interaction::shapes::responses::response_not_found)?;
    Ok(ConversationRef {
        channel: "api".into(),
        id,
        route: Default::default(),
    })
}

/// D109: whether a runtime request can be answered at all — 409 `job_not_live`, 503 `unavailable` (no runtime
/// process serves the scope) — checked before a stream starts so a refusal is the normal error JSON.
pub async fn runtime_ready(st: &AppState, req: &InteractionRequest) -> Result<(), BackendError> {
    let scope = JobScope {
        tenant: req.tenant,
        job_id: req.job_id,
        job_key: req.job_key.clone(),
    };
    let ready = async {
        let mut tx = store::begin_job(&st.db, scope.tenant, scope.job_id).await?;
        job_live(st, &mut tx, &scope).await?;
        tx.commit().await?;
        RuntimeBackend::serving(st, &scope, CUSTOMER_ENV)
            .await
            .map(|_| ())
            .ok_or_else(turn::unavailable)
    };
    ready.await.map_err(backend_error)
}

/// D109/A4: one request with a party of the interaction API as a runtime turn (module doc): 409 `job_not_live`, 503
/// `unavailable` (not served, or no reply within [`REPLY_WAIT`]), 409 `conversation_busy` / `conversation_ended`.
/// The reply text; empty when a person answers (a handover queued, staff in control).
pub async fn runtime_turn(st: &AppState, req: &InteractionRequest) -> ApiResult<String> {
    let scope = JobScope {
        tenant: req.tenant,
        job_id: req.job_id,
        job_key: req.job_key.clone(),
    };
    let text = req
        .input
        .iter()
        .rev()
        .find(|m| m.role == Role::User)
        .map(Message::joined)
        .unwrap_or_default();
    if text.trim().is_empty() {
        return Err(ApiError::Unprocessable(
            "empty_input",
            "the request carries no user message".into(),
        ));
    }
    let author_role = match req.party.as_ref().map(|p| p.role) {
        Some(PartyRole::System) => AuthorRole::Caller,
        _ => AuthorRole::User,
    };
    let (party_id, party_name) = match &req.party {
        Some(p) => (p.id.clone(), p.display_name.clone()),
        None => (format!("int:{}", req.id), None),
    };
    let author = party_name.clone().unwrap_or_else(|| party_id.clone());
    let channel = conversation_channel(req);
    let cref = ConversationRef {
        channel: channel.to_string(),
        id: req
            .conversation
            .as_ref()
            .map_or_else(|| req.id.to_string(), |c| c.id.clone()),
        route: req
            .conversation
            .as_ref()
            .map(|c| c.route.clone())
            .unwrap_or_default(),
    };
    let external_id = req.id.to_string();
    // read before any lock is taken
    let runtime = RuntimeBackend::serving(st, &scope, CUSTOMER_ENV).await;
    let mut tx = store::begin_job(&st.db, scope.tenant, scope.job_id).await?;
    job_live(st, &mut tx, &scope).await?;
    let rt = runtime.ok_or_else(turn::unavailable)?;
    store::lock(
        &mut tx,
        &format!("{}:{}:{channel}:{}", scope.tenant, scope.job_id, cref.id),
    )
    .await?;
    let conv = store::upsert_customer_conversation(
        &mut tx,
        scope.job_id,
        channel,
        &cref,
        &party_id,
        party_name.as_deref(),
        CUSTOMER_ENV,
    )
    .await?;
    // the same request again (an async interaction claimed twice): wait for its turn, never a second one
    let seen: Option<(Uuid, i64)> = sqlx::query_as(
        "SELECT message_id, at_ms FROM conversation_message \
          WHERE conversation_id = $1 AND external_id = $2 AND role IN ('customer', 'caller')",
    )
    .bind(&conv.id)
    .bind(&external_id)
    .fetch_optional(&mut *tx)
    .await?;
    if seen.is_none() {
        seed_history(&mut tx, &scope, &conv.id, &author, req).await?;
        record_caller(&mut tx, &scope, &conv.id, req).await?;
    }
    record_callback(&mut tx, &scope, &conv.id, req.id).await?;
    tx.commit().await?;
    let outcome = match seen {
        Some((message_id, at_ms)) => rt.reply_after(&scope, &conv.id, at_ms, message_id).await?,
        None => {
            rt.turn_as(
                Inbound {
                    scope: &scope,
                    conv: &conv,
                    author: &author,
                    actor: Actor::system(SERVICE),
                    text: &text,
                    external_id: Some(&external_id),
                },
                author_role,
            )
            .await?
        }
    };
    Ok(match outcome {
        Outcome::Replied { text, .. } => text,
        Outcome::Queued { .. } | Outcome::StaffControl => String::new(),
    })
}

/// An async request's `callback_url` (read from its delivery row, which intake validated) becomes the conversation's
/// `callback_url` state; the latest wins. A task message the relay posts later is delivered there
/// (crate::interaction::webhook::notify_message). A sync request writes nothing and keeps the earlier URL.
async fn record_callback(
    tx: &mut PgConnection,
    scope: &JobScope,
    conversation_id: &str,
    request: Uuid,
) -> ApiResult<()> {
    let url: Option<String> = sqlx::query_scalar(
        "SELECT callback_url FROM interaction_deliveries WHERE id = $1 AND job_id = $2",
    )
    .bind(request)
    .bind(scope.job_id)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(url) = url {
        conv_state::put_value(
            &mut *tx,
            conversation_id,
            conv_state::CALLBACK_URL,
            &url,
            "interaction",
            None,
        )
        .await?;
    }
    Ok(())
}

/// The API key behind a `system` party `api:<key id>` of this job, as the conversation's `caller`, `caller_label` and
/// `identity_source = api_key` state (A4; the caller of the latest message). Any other party writes nothing here. The
/// label is the tenant's own text and is stored, never logged.
async fn record_caller(
    tx: &mut PgConnection,
    scope: &JobScope,
    conversation_id: &str,
    req: &InteractionRequest,
) -> ApiResult<()> {
    let Some(p) = req.party.as_ref().filter(|p| p.role == PartyRole::System) else {
        return Ok(());
    };
    let Some(key) =
        p.id.strip_prefix("api:")
            .and_then(|k| Uuid::parse_str(k).ok())
    else {
        return Ok(());
    };
    let label: Option<String> =
        sqlx::query_scalar("SELECT label FROM job_api_keys WHERE id = $1 AND job_id = $2")
            .bind(key)
            .bind(scope.job_id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some(label) = label else {
        return Ok(());
    };
    let by = format!("api_key:{key}");
    for (k, v) in [
        (conv_state::CALLER, key.to_string()),
        (conv_state::CALLER_LABEL, label),
        (
            conv_state::IDENTITY_SOURCE,
            conv_state::SOURCE_API_KEY.to_string(),
        ),
    ] {
        conv_state::put_value(&mut *tx, conversation_id, k, &v, &by, None).await?;
    }
    Ok(())
}

/// A stateless request's earlier turns (every `input` message before the last user message; system messages are
/// instructions, not turns) become the opening lines of its NEW conversation — customer lines redacted (N-03), agent
/// lines as `agent` — so the runtime runs on the context the caller sent. A conversation that already has lines (a
/// connector's own `ConversationRef`) keeps its stored history and is not seeded.
async fn seed_history(
    tx: &mut PgConnection,
    scope: &JobScope,
    conversation_id: &str,
    author: &str,
    req: &InteractionRequest,
) -> ApiResult<()> {
    let Some(last) = req.input.iter().rposition(|m| m.role == Role::User) else {
        return Ok(());
    };
    if last == 0 {
        return Ok(());
    }
    let has_lines: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM conversation_message WHERE conversation_id = $1)",
    )
    .bind(conversation_id)
    .fetch_one(&mut *tx)
    .await?;
    if has_lines {
        return Ok(());
    }
    for m in &req.input[..last] {
        let (role, line_author, text) = match m.role {
            Role::User => ("customer", author, turn::redact::redact(&m.joined()).text),
            Role::Assistant => ("agent", "agent", m.joined()),
            Role::System => continue,
        };
        if text.trim().is_empty() {
            continue;
        }
        store::append(
            &mut *tx,
            store::NewMessage {
                job_id: scope.job_id,
                conversation_id,
                role,
                author: line_author,
                text: &text,
                case_id: None,
                external_id: None,
            },
        )
        .await?;
    }
    Ok(())
}

/// [`runtime_turn`] as the stream of the interaction seam: the reply as one delta, then done; or the turn's error.
pub fn runtime_stream(
    st: AppState,
    req: InteractionRequest,
) -> BoxStream<'static, Result<BackendEvent, BackendError>> {
    stream::once(async move {
        let events = match runtime_turn(&st, &req).await {
            Ok(text) if text.is_empty() => vec![Ok(BackendEvent::Done { usage: None })],
            Ok(text) => vec![
                Ok(BackendEvent::Delta(text)),
                Ok(BackendEvent::Done { usage: None }),
            ],
            Err(e) => vec![Err(backend_error(e))],
        };
        stream::iter(events)
    })
    .flatten()
    .boxed()
}

/// The stored customer message `external_id` of `conversation_id` when the runtime qualified its case `case_id`
/// (a `qualified` case event: the B12 path never writes one), as `(message_id, at_ms)`. A provider retry of such a
/// message waits for the runtime's reply instead of answering it a second time.
pub async fn runtime_message(
    conn: &mut PgConnection,
    conversation_id: &str,
    external_id: &str,
    case_id: &str,
) -> ApiResult<Option<(Uuid, i64)>> {
    let row: Option<(Uuid, i64)> = sqlx::query_as(
        "SELECT m.message_id, m.at_ms FROM conversation_message m \
          WHERE m.conversation_id = $1 AND m.external_id = $2 \
            AND EXISTS (SELECT 1 FROM case_event e WHERE e.case_id = $3 AND e.kind = 'qualified')",
    )
    .bind(conversation_id)
    .bind(external_id)
    .bind(case_id)
    .fetch_optional(conn)
    .await?;
    Ok(row)
}

/// A turn error as the backend error a stream carries (only the status and code are logged): 409 `job_not_live`
/// stays itself, anything else is `unavailable`.
fn backend_error(e: ApiError) -> BackendError {
    let (status, code) = e.status_and_code();
    if code == "job_not_live" {
        return BackendError::NotLive(e.to_string());
    }
    tracing::warn!(
        status = status.as_u16(),
        code,
        "runtime_backend_turn_failed"
    );
    BackendError::Unavailable("the runtime could not take the turn".into())
}

/// Every request is a runtime turn ([`runtime_stream`]): there is no fallback backend (D109).
impl InteractionBackend for RuntimeBackend {
    fn respond(
        &self,
        req: InteractionRequest,
    ) -> BoxStream<'static, Result<BackendEvent, BackendError>> {
        runtime_stream(self.st.clone(), req)
    }
}
