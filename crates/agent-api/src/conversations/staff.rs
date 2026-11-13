//! THE one staff-message path (SW T3): the Send route (`POST /api/jobs/{job}/conversations/{id}/messages`, kind
//! `reply` or `note`) and the worker-thread door (crate::conversations::internal, through [`reply`]) both call
//! [`send`]. In one transaction: a retry of the same `client_msg_id` answers the stored message and does nothing else;
//! a reply's gate (the caller owns the conversation, crate::conversations::ownership: 403 `not_owner`; a retry of a
//! failed reply is its author's, while the author, the agent or the queue owns the conversation), the message,
//! the audit row, and for a reply `first_response_at` and `assigned → waiting_customer` on the conversation's open
//! item. After the commit: the `message` frame (with the `MessageView` fields, `client_msg_id` included), then a
//! reply's delivery to the customer's channel through agent-edge (web chat: what the edge accepted is then published
//! on the conversation's customer stream, crate::conversations::webchat_stream). A failed delivery keeps the message
//! with its `delivery_error`, publishes `delivery_failed` `{conversation_id, message_id, code}` and is
//! `delivery: "failed"` in the answer (the route's 202).
//!
//! A note (role `note`) is staff-only: it skips the owner gate, is never delivered to the customer's channel and never
//! moves the item's state or `first_response_at`. SW-11: after its commit, a note on a conversation whose open handover
//! item has a worker-side HELPDESK thread (the Frappe ticket) is mirrored to that ticket as a private comment
//! (`Note from {author}: {text}`, agent-edge `POST /internal/worker-note`), best-effort and off the request path: a
//! failure is logged and the note stays stored. Helpdesk only.
//!
//! A reply is a staff interaction on the B10 seam: an [`InteractionRequest`] with a `PartyRole::Staff` party and the
//! customer conversation's [`ConversationRef`] is built and validated (its limits apply), and never sent to the agent.
use serde_json::{Value, json};
use sqlx::Row;
use uuid::Uuid;

use super::edge_link::DeliverKind;
use super::handover::{self, State};
use super::hub::Frame;
use super::note_mirror;
use super::ownership::{self, Owner};
use super::store::{self, MessageView};
use super::webchat_stream;
use crate::attachments::store::{AttachmentStore, PgAttachmentStore};
use crate::audit::{self, Actor, Event};
use crate::error::{ApiError, ApiResult};
use crate::interaction::{
    ConversationRef, InteractionRequest, Message, Party, PartyRole, Role, Scope,
};
use crate::runtime::id_guard::{self, ReplyOrigin};
use crate::runtime::{Env, Scope as BusScope};
use crate::sse_hub;
use crate::state::AppState;

/// What a staff message is: a `reply` to the customer (role `staff`) or a staff-only `note` (role `note`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MsgKind {
    Reply,
    Note,
}

impl MsgKind {
    /// The wire kind (`reply` | `note`); `None` for anything else.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "reply" => Some(MsgKind::Reply),
            "note" => Some(MsgKind::Note),
            _ => None,
        }
    }

    /// The `conversation_message.role` it is stored as.
    pub fn role(self) -> &'static str {
        match self {
            MsgKind::Reply => "staff",
            MsgKind::Note => "note",
        }
    }
}

/// One staff message.
#[derive(Clone, Debug)]
pub struct Reply {
    pub scope: Scope,
    pub conversation_id: String,
    pub kind: MsgKind,
    pub actor: Actor,
    pub sub: String,
    pub name: String,
    /// Checked by the caller ([`check_text`]).
    pub text: String,
    /// The worker channel's message id (a retry is not stored twice).
    pub external_id: Option<String>,
    /// The sender's id of one Send (a retry with the same id answers the stored message).
    pub client_msg_id: Option<Uuid>,
    /// SW-9: unsent attachments (uploaded to this conversation) the reply carries, at most [`MAX_ATTACHMENTS`]; they
    /// are linked to the message in the transaction that stores it. A note carries none.
    pub attachments: Vec<Uuid>,
}

/// The most attachments one staff reply carries (SW-9 design §2).
pub const MAX_ATTACHMENTS: usize = 5;

/// How a reply ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Delivered to the customer's channel.
    Delivered,
    /// Stored only: no edge configured, a channel without a ref (staff / test chat), a note, a duplicate, or a failed
    /// delivery ([`Sent::failed`]).
    Stored,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Delivered => "delivered",
            Outcome::Stored => "stored",
        }
    }
}

/// A staff message as [`send`] left it.
#[derive(Clone, Debug)]
pub struct Sent {
    /// The message now (a failed delivery: `delivery == "failed"`).
    pub view: MessageView,
    pub outcome: Outcome,
    /// The edge's code when this call's delivery failed (`delivery_failed`, …).
    pub failed: Option<&'static str>,
}

/// The most characters a staff reply on `channel` may carry: the channel connector's own message limit (Telegram
/// [`crate::channels::telegram::MAX_TEXT_CHARS`]), else the
/// stored message limit ([`store::MAX_TEXT`]).
pub fn channel_limit(channel: &str) -> usize {
    match channel {
        "telegram" => crate::channels::telegram::MAX_TEXT_CHARS,
        _ => store::MAX_TEXT,
    }
}

/// The trimmed text, else 422 `empty_message` (nothing but blanks) or 422 `message_too_long` with the `limit`.
pub fn check_text(text: &str, limit: usize) -> ApiResult<String> {
    store::require_clean([text])?;
    let t = text.trim();
    if t.is_empty() {
        return Err(ApiError::Unprocessable(
            "empty_message",
            "Write a message first.".into(),
        ));
    }
    if t.chars().count() > limit {
        return Err(ApiError::Unprocessable(
            "message_too_long",
            format!("A message here carries at most {limit} characters."),
        )
        .with("limit", limit));
    }
    Ok(t.to_string())
}

/// [`check_text`] for a staff reply: a reply that carries attachments (SW-9) may have no text at all (`""`); one
/// without attachments, and every note, still needs text.
pub fn check_reply_text(text: &str, limit: usize, has_attachments: bool) -> ApiResult<String> {
    if has_attachments && text.trim().is_empty() {
        store::require_clean([text])?;
        return Ok(String::new());
    }
    check_text(text, limit)
}

/// 403 `not_owner`: only the conversation's owner replies to the customer.
fn not_owner() -> ApiError {
    ApiError::Forbidden("not_owner", "Take this conversation to reply.".into())
}

/// The worker-thread door's reply (crate::conversations::internal): [`send`] a reply, a failed delivery as 502
/// `delivery_failed` (the message is kept; a retry of the same worker message delivers it again).
pub async fn reply(st: &AppState, r: Reply) -> ApiResult<Outcome> {
    let sent = send(st, r).await?;
    match sent.failed {
        Some(code) => Err(delivery_failed(code)),
        None => Ok(sent.outcome),
    }
}

fn delivery_failed(_code: &str) -> ApiError {
    ApiError::BadGateway(
        "delivery_failed",
        "The reply is saved but did not reach the customer's channel.".into(),
    )
}

/// Send one staff message (see the module doc).
pub async fn send(st: &AppState, r: Reply) -> ApiResult<Sent> {
    let s = &r.scope;
    let mut tx = store::begin_job(&st.db, s.tenant, s.job_id).await?;
    let conv = store::conversation_or_404(&mut tx, &r.conversation_id).await?;
    // a retry of a stored Send: the message as it is now, nothing stored twice (nor is the owner asked again); only a
    // reply whose delivery failed is delivered again, by its author while the author, the agent or the queue owns the
    // conversation (a failed closing message of a Resolve is retried after the agent got the conversation back)
    if let Some(id) = r.client_msg_id
        && let Some(view) = store::message_by_client(&mut tx, &conv.id, id).await?
    {
        if !(view.kind == "staff" && view.delivery == Some("failed")) {
            tx.commit().await?;
            return Ok(Sent {
                view,
                outcome: Outcome::Stored,
                failed: None,
            });
        }
        store::lock(&mut tx, &store::control_key(&conv.id)).await?;
        let c = ownership::lock(&mut tx, &conv.id).await?;
        let author = reply_author(&mut tx, &conv.id, view.message_id).await?;
        let mine = author.as_deref() == Some(r.sub.as_str());
        let open_to_author = match &c.owner {
            Owner::User(sub) => *sub == r.sub,
            Owner::Agent | Owner::Queue => true,
        };
        if !(mine && open_to_author) {
            return Err(not_owner());
        }
        let cref = store::customer_ref(&mut tx, &conv.id).await?;
        let last = last_customer_at_ms(&mut tx, &conv.id).await?;
        tx.commit().await?;
        let text = view.text.clone();
        return deliver(
            st,
            s,
            &conv.id,
            view,
            &text,
            customer_facing_name(&r.name, &r.sub),
            cref,
            last,
        )
        .await;
    }
    let item = match r.kind {
        MsgKind::Reply => {
            // the seam's staff interaction: validated, never answered by the agent
            // an attachment-only reply has no text; the seam's validation needs some input, so it sees a stand-in
            // (only the stored text, which stays empty, is the customer's)
            let seen = if r.text.is_empty() {
                "(attachment)".to_string()
            } else {
                r.text.clone()
            };
            let mut req = InteractionRequest::new(s, vec![Message::text(Role::Assistant, seen)]);
            req.channel = conv.channel.clone();
            req.party = Some(Party {
                id: r.sub.clone(),
                display_name: Some(r.name.clone()),
                role: PartyRole::Staff,
            });
            req.conversation = Some(ConversationRef {
                channel: conv.channel.clone(),
                id: conv.id.clone(),
                route: Default::default(),
            });
            req.validate()?;
            // the owner gate, in the ownership commands' lock order: control lock, owner row, item
            store::lock(&mut tx, &store::control_key(&conv.id)).await?;
            let c = ownership::lock(&mut tx, &conv.id).await?;
            if c.owner != Owner::User(r.sub.clone()) {
                return Err(not_owner());
            }
            handover::open_for_conversation(&mut tx, &conv.id).await?
        }
        MsgKind::Note => None,
    };
    let case_id = match &item {
        Some(it) => Some(it.case_id.clone()),
        None => store::open_case(&mut tx, &conv.id).await?.map(|(c, _)| c),
    };
    let stored = store::append_client(
        &mut tx,
        store::NewMessage {
            job_id: s.job_id,
            conversation_id: &conv.id,
            role: r.kind.role(),
            author: &r.name,
            text: &r.text,
            case_id: case_id.as_deref(),
            external_id: r.external_id.as_deref(),
        },
        r.client_msg_id,
        Some(r.sub.as_str()),
    )
    .await?;
    let Some(stored) = stored else {
        // the same Send stored by a concurrent call (it committed first): that message
        if let Some(id) = r.client_msg_id
            && let Some(view) = store::message_by_client(&mut tx, &conv.id, id).await?
        {
            tx.commit().await?;
            return Ok(Sent {
                view,
                outcome: Outcome::Stored,
                failed: None,
            });
        }
        return retried(st, s, tx, &conv.id, &r).await;
    };
    if !r.attachments.is_empty() {
        // all or nothing, in this transaction: an unknown, foreign, deleted or already sent id is the uniform 404 and
        // the message is not stored
        PgAttachmentStore::for_state(st)
            .link_to_message(&mut tx, &conv.id, stored.message_id, &r.attachments)
            .await?;
    }
    if r.kind == MsgKind::Reply
        && let Some(it) = &item
    {
        sqlx::query(
            "UPDATE agent_handover_items SET first_response_at = COALESCE(first_response_at, now()) WHERE id = $1",
        )
        .bind(&it.id)
        .execute(&mut *tx)
        .await?;
        if it.state == State::Assigned {
            handover::move_to(
                &mut tx,
                s.job_id,
                it,
                State::WaitingCustomer,
                &r.sub,
                "replied",
            )
            .await?;
        }
    }
    let event = match r.kind {
        MsgKind::Reply => Event::new("conversation.staff_reply", "conversation", conv.id.clone()),
        MsgKind::Note => Event::new("conversation.note", "conversation", conv.id.clone()),
    };
    let mut event = event
        .job(&s.job_key)
        .detail("conversation_id", conv.id.clone())
        .detail("channel", conv.channel.clone())
        .detail("message_id", stored.message_id.to_string());
    if let Some(it) = &item {
        event = event.detail("item_id", it.id.clone());
    }
    audit::record(&mut tx, &r.actor, event).await?;
    // SW-11: a note under an open item is queued for the item's helpdesk ticket in its own transaction (0206)
    let mirrored = r.kind == MsgKind::Note
        && st.edge.is_some()
        && note_mirror::enqueue(&mut tx, s.job_id, &conv.id, stored.message_id).await?;
    let (view, frame) = stored_frame(&mut tx, &conv.id, stored.message_id).await?;
    let target = match r.kind {
        MsgKind::Reply => Some((
            store::customer_ref(&mut tx, &conv.id).await?,
            last_customer_at_ms(&mut tx, &conv.id).await?,
        )),
        MsgKind::Note => None,
    };
    tx.commit().await?;
    publish(st, s, &conv.id, "message", frame).await;
    let Some((cref, last)) = target else {
        // a note never reaches the customer; the helpdesk ticket gets it as a private comment (SW-11), first try now
        if mirrored {
            note_mirror::mirror_now(st, s.tenant, s.job_id, stored.message_id);
        }
        return Ok(Sent {
            view,
            outcome: Outcome::Stored,
            failed: None,
        });
    };
    // the id-shape guard (log-only, owner decision 2026-10-27): a staff reply is customer-facing; a note never gets here
    id_guard::observe(
        &ReplyOrigin {
            tenant: s.tenant,
            job: &s.job_key,
            conversation: &conv.id,
            case: case_id.as_deref(),
            channel: &conv.channel,
        },
        &r.text,
    );
    let name = customer_facing_name(&r.name, &r.sub);
    deliver(st, s, &conv.id, view, &r.text, name, cref, last).await
}

/// The sub that sent staff reply `message_id`: its `author_sub` (0192; `author` names the author by display name
/// only). `None` for a row stored before 0192.
async fn reply_author(
    conn: &mut sqlx::PgConnection,
    conversation_id: &str,
    message_id: Uuid,
) -> ApiResult<Option<String>> {
    let sub: Option<Option<String>> = sqlx::query_scalar(
        "SELECT author_sub FROM conversation_message WHERE conversation_id = $1 AND message_id = $2",
    )
    .bind(conversation_id)
    .bind(message_id)
    .fetch_optional(conn)
    .await?;
    Ok(sub.flatten())
}

/// A worker message stored before (its `external_id`): delivered again only when it never reached the customer.
async fn retried(
    st: &AppState,
    s: &Scope,
    mut tx: sqlx::Transaction<'static, sqlx::Postgres>,
    conversation_id: &str,
    r: &Reply,
) -> ApiResult<Sent> {
    let prev: Option<Uuid> = sqlx::query_scalar(
        "SELECT message_id FROM conversation_message \
          WHERE conversation_id = $1 AND external_id = $2 \
          ORDER BY (role = 'staff' AND delivered_at IS NULL) DESC LIMIT 1",
    )
    .bind(conversation_id)
    .bind(r.external_id.as_deref())
    .fetch_optional(&mut *tx)
    .await?;
    // unreachable in practice: `append` refused the insert on a unique conflict, the client id case was handled by the
    // caller, and the only other unique key a staff message carries is this `external_id`, so the conflicting row
    // exists (rows are never deleted)
    let Some(prev) = prev else {
        return Err(ApiError::Internal(anyhow::anyhow!(
            "a staff message conflicted with no stored message"
        )));
    };
    let view = store::message_view(&mut tx, prev).await?;
    let cref = store::customer_ref(&mut tx, conversation_id).await?;
    let last = last_customer_at_ms(&mut tx, conversation_id).await?;
    tx.commit().await?;
    if view.kind != "staff" || view.delivery == Some("delivered") {
        return Ok(Sent {
            view,
            outcome: Outcome::Stored,
            failed: None,
        });
    }
    let text = view.text.clone();
    deliver(
        st,
        s,
        conversation_id,
        view,
        &text,
        customer_facing_name(&r.name, &r.sub),
        cref,
        last,
    )
    .await
}

/// The `message` frame of a stored staff message: the B12 fields (`role`, `author`, `at_ms`) and the `MessageView`
/// ones (`kind`, `event`, `author_name`, `at`, `client_msg_id`, `delivery`), so a client reconciles its optimistic row
/// by `client_msg_id` whichever of this frame or the Send's answer arrives first.
pub(crate) fn message_frame(
    conversation_id: &str,
    v: &MessageView,
    author: &str,
    at_ms: i64,
) -> Value {
    let mut f = serde_json::to_value(v).unwrap_or_else(|_| json!({}));
    f["conversation_id"] = json!(conversation_id);
    f["role"] = json!(v.kind);
    f["author"] = json!(author);
    f["at_ms"] = json!(at_ms);
    f
}

/// Message `message_id` as it is stored now: its [`MessageView`] and its `message` frame ([`message_frame`]).
async fn stored_frame(
    conn: &mut sqlx::PgConnection,
    conversation_id: &str,
    message_id: Uuid,
) -> ApiResult<(MessageView, Value)> {
    let r = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {} FROM conversation_message WHERE message_id = $1",
        store::MESSAGE_VIEW_COLS
    )))
    .bind(message_id)
    .fetch_one(&mut *conn)
    .await?;
    let mut view = MessageView::of_row(&r);
    store::with_attachments(&mut *conn, std::slice::from_mut(&mut view)).await?;
    let author: String = r.get::<Option<String>, _>("author").unwrap_or_default();
    let at_ms: i64 = r.get("at_ms");
    let frame = message_frame(conversation_id, &view, &author, at_ms);
    Ok((view, frame))
}

/// The staff author's name as the customer may see it: the console's name of the author ([`Reply::name`]: the
/// member's display name, or a platform super_admin's 0115 `<preferred_username> (platform)`), but never an email
/// (a member without a display name is named by its email) or the Keycloak sub (no name at all): those are `None`.
pub fn customer_facing_name<'a>(name: &'a str, sub: &str) -> Option<&'a str> {
    let n = name.trim();
    (!n.is_empty() && n != sub.trim() && !n.contains('@')).then_some(n)
}

/// The customer's newest message in the conversation (unix ms); `None` when the customer never wrote.
pub async fn last_customer_at_ms(
    conn: &mut sqlx::PgConnection,
    conversation_id: &str,
) -> ApiResult<Option<i64>> {
    let last: Option<i64> = sqlx::query_scalar(
        "SELECT max(at_ms) FROM conversation_message WHERE conversation_id = $1 AND role = 'customer'",
    )
    .bind(conversation_id)
    .fetch_one(conn)
    .await?;
    Ok(last)
}

/// Deliver a stored staff reply (`view`) to the customer's channel, stamp `delivered_at` (clearing `delivery_error`)
/// and publish the `message` frame again with `delivery: "delivered"`, so every staff viewer sees it; a failure keeps
/// the message undelivered with its `delivery_error` (a retry of the same worker message or Send delivers it again)
/// and publishes `delivery_failed` `{conversation_id, message_id, code}` with the edge's code. No edge configured, or no customer ref: stored
/// only.
#[allow(clippy::too_many_arguments)]
async fn deliver(
    st: &AppState,
    s: &Scope,
    conversation_id: &str,
    mut view: MessageView,
    text: &str,
    name: Option<&str>,
    cref: Option<(ConversationRef, Option<Uuid>)>,
    last_customer_at_ms: Option<i64>,
) -> ApiResult<Sent> {
    let (Some(edge), Some((cref, binding))) = (st.edge.clone(), cref) else {
        return Ok(Sent {
            view,
            outcome: Outcome::Stored,
            failed: None,
        });
    };
    match edge
        .deliver_with(
            &cref,
            DeliverKind::Text,
            text,
            name,
            &view.attachments,
            binding,
            last_customer_at_ms,
        )
        .await
    {
        Ok(external_id) => {
            webchat_stream::published_with_attachments(
                st,
                s,
                conversation_id,
                &cref,
                DeliverKind::Text,
                text,
                name,
                &external_id,
                &view.attachments,
            )
            .await;
            let mut tx = store::begin_job(&st.db, s.tenant, s.job_id).await?;
            sqlx::query(
                "UPDATE conversation_message SET delivered_at = now(), delivery_error = NULL \
                  WHERE message_id = $1",
            )
            .bind(view.message_id)
            .execute(&mut *tx)
            .await?;
            let (view, frame) = stored_frame(&mut tx, conversation_id, view.message_id).await?;
            tx.commit().await?;
            publish(st, s, conversation_id, "message", frame).await;
            Ok(Sent {
                view,
                outcome: Outcome::Delivered,
                failed: None,
            })
        }
        Err(e) => {
            tracing::warn!(
                conversation = conversation_id,
                code = e.code,
                "staff_reply_delivery_failed"
            );
            // the failure is reported even when it cannot be recorded on the row
            let marked = async {
                let mut tx = store::begin_job(&st.db, s.tenant, s.job_id).await?;
                sqlx::query(
                    "UPDATE conversation_message SET delivery_error = $2 WHERE message_id = $1",
                )
                .bind(view.message_id)
                .bind(e.code)
                .execute(&mut *tx)
                .await?;
                tx.commit().await?;
                ApiResult::Ok(())
            }
            .await;
            if let Err(err) = marked {
                tracing::warn!(
                    conversation = conversation_id,
                    message = %view.message_id,
                    error = err.code(),
                    "staff_reply_delivery_error_not_stored"
                );
            }
            publish(
                st,
                s,
                conversation_id,
                "delivery_failed",
                json!({"conversation_id": conversation_id, "message_id": view.message_id, "code": e.code}),
            )
            .await;
            view.delivery = Some("failed");
            Ok(Sent {
                view,
                outcome: Outcome::Stored,
                failed: Some(e.code),
            })
        }
    }
}

/// Publish one frame of the job's feeds.
pub async fn publish(
    st: &AppState,
    s: &Scope,
    conversation_id: &str,
    event: &'static str,
    data: serde_json::Value,
) {
    // through Redis when it is configured (every replica's feeds, this one's via the bridge: once), else this
    // process's hub (crate::sse_hub::publish); the env is not part of the route, the bridge subscribes both
    match BusScope::new(s.tenant, s.job_key.clone(), Env::Prod) {
        Ok(bus) => sse_hub::publish(st, &bus, s.job_id, conversation_id, event, data).await,
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "staff_publish_scope_invalid");
            st.events.publish(Frame {
                tenant: s.tenant,
                job_id: s.job_id,
                conversation_id: Some(conversation_id.to_string()),
                event,
                data,
            });
        }
    }
}

/// Deliver a notice (`handover_notice`, `csat_request`) to the conversation's customer channel, best effort: a
/// failure is logged and published, never an error of the request that caused it. Returns `false` only when a
/// retry could help (the customer lookup or the edge's delivery failed); `true` when delivered, or when there is
/// nothing to deliver to (no edge configured, no customer ref). Callers may ignore it.
pub async fn notify_customer(
    st: &AppState,
    s: &Scope,
    conversation_id: &str,
    kind: DeliverKind,
    text: &str,
) -> bool {
    let Some(edge) = st.edge.clone() else {
        return true;
    };
    let cref = async {
        let mut tx = store::begin_job(&st.db, s.tenant, s.job_id).await?;
        let r = store::customer_ref(&mut tx, conversation_id).await?;
        let last = last_customer_at_ms(&mut tx, conversation_id).await?;
        tx.commit().await?;
        ApiResult::Ok(r.map(|(c, b)| (c, b, last)))
    }
    .await;
    let (cref, binding, last) = match cref {
        Ok(Some(found)) => found,
        Ok(None) => return true,
        Err(_) => return false,
    };
    match edge.deliver(&cref, kind, text, None, binding, last).await {
        Ok(external_id) => {
            webchat_stream::published(
                st,
                s,
                conversation_id,
                &cref,
                kind,
                text,
                None,
                &external_id,
            )
            .await;
            true
        }
        Err(e) => {
            tracing::warn!(
                conversation = conversation_id,
                code = e.code,
                "customer_notice_delivery_failed"
            );
            publish(
                st,
                s,
                conversation_id,
                "delivery_failed",
                json!({"conversation_id": conversation_id, "code": e.code}),
            )
            .await;
            false
        }
    }
}
