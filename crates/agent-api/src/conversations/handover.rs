//! The handover queue (D64, B4 T16): items `HO-n` in `agent_handover_items`, every transition and (re)assignment in
//! `agent_handover_events`, one open item per case. The state machine is the web's `J21Handover.ts` `TRANSITIONS` /
//! `applyAssign`: `queued → assigned → waiting_customer → resolved | returned_to_agent`, `waiting_customer → assigned`
//! when the customer answers. Assignees are enabled members holding `support_agent` or `support_manager` (D88); a super_admin may take an
//! item for itself. Every change runs in one transaction with `FOR UPDATE` on the item.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{PgConnection, Row};
use uuid::Uuid;

use super::edge_link::{ExcerptLine, NotifyWorkersReq, WorkerItem, WorkerTarget};
use super::ownership::{self, Owner};
use super::{iso, store};
use crate::design::CaseDesign;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Queued,
    Assigned,
    WaitingCustomer,
    Resolved,
    ReturnedToAgent,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Queued => "queued",
            State::Assigned => "assigned",
            State::WaitingCustomer => "waiting_customer",
            State::Resolved => "resolved",
            State::ReturnedToAgent => "returned_to_agent",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        [
            State::Queued,
            State::Assigned,
            State::WaitingCustomer,
            State::Resolved,
            State::ReturnedToAgent,
        ]
        .into_iter()
        .find(|x| x.as_str() == s)
    }

    pub fn closed(self) -> bool {
        matches!(self, State::Resolved | State::ReturnedToAgent)
    }

    /// `J21Handover.ts` `TRANSITIONS`.
    pub fn allows(self, to: State) -> bool {
        use State::*;
        match self {
            Queued => matches!(to, Assigned | ReturnedToAgent),
            Assigned => matches!(to, WaitingCustomer | Resolved | ReturnedToAgent),
            WaitingCustomer => matches!(to, Assigned | Resolved | ReturnedToAgent),
            Resolved | ReturnedToAgent => false,
        }
    }
}

/// Why `from → to` is refused (`transitionError` of the web plus `assign_first`), in the B4 order.
pub fn transition_error(from: State, to: State) -> Option<&'static str> {
    if from.closed() {
        return Some("closed");
    }
    if from == State::Queued && to != State::ReturnedToAgent {
        return Some("assign_first");
    }
    if from == to {
        return Some("same_state");
    }
    if !from.allows(to) {
        return Some("invalid_transition");
    }
    None
}

/// The D64 reasons (`HandoverReason` of the web plus `staff_decision` D85, `gate_unavailable` D94, and 0179's
/// orchestrator reasons `not_offered` and `success_criteria_unmet`, and 0183's `step_up_required`: a step needs
/// identity level 3 and a colleague confirms it).
pub const REASONS: [&str; 11] = [
    "identity_failed",
    "gate_blocked",
    "customer_asked",
    "tool_failed",
    "unknown_outcome",
    "out_of_scope",
    "staff_decision",
    "gate_unavailable",
    "not_offered",
    "success_criteria_unmet",
    "step_up_required",
];

/// What the state machine reads of an item. `assignee` is derived (0200 `agent_handover_items_owned`, SW-2): the
/// conversation's staff owner while the item is open, else who closed it.
#[derive(Clone, Debug)]
pub struct Item {
    pub id: String,
    pub case_id: String,
    pub conversation_id: String,
    pub state: State,
    pub assignee: Option<String>,
}

fn item_of(r: &sqlx::postgres::PgRow) -> ApiResult<Item> {
    let s: String = r.get("state");
    Ok(Item {
        id: r.get("id"),
        case_id: r.get("case_id"),
        conversation_id: r.get("conversation_id"),
        state: State::parse(&s)
            .ok_or_else(|| ApiError::Internal(anyhow::anyhow!("unknown handover state")))?,
        assignee: r.get("assignee"),
    })
}

/// The item columns of [`Item`], from the base table `h` joined to its owned view `o` (the derived assignee). A lock
/// is `FOR UPDATE OF h`: only the item row (the runtime role may not lock `conversation`; the owner row is locked
/// first, on its own, by ownership::guard / ownership::lock).
const ITEM_COLS: &str = "h.id, h.case_id, h.conversation_id, h.state, o.assignee";
const ITEM_FROM: &str = "agent_handover_items h JOIN agent_handover_items_owned o \
     ON o.tenant_id = h.tenant_id AND o.job_id = h.job_id AND o.id = h.id";

/// The derived assignee of item `id`, read again after the item row is locked: a statement started after the lock
/// sees an owner change committed while this transaction waited for it.
async fn fresh(conn: &mut PgConnection, mut item: Item) -> ApiResult<Item> {
    let a: Option<Option<String>> =
        sqlx::query_scalar("SELECT assignee FROM agent_handover_items_owned WHERE id = $1")
            .bind(&item.id)
            .fetch_optional(conn)
            .await?;
    item.assignee = a.flatten();
    Ok(item)
}

pub fn not_found() -> ApiError {
    // The UI branches on this code for an unknown item (B4: `handoverOf`).
    ApiError::Missing(
        "case_not_found",
        "No such handover item in this job.".into(),
    )
}

/// The item, locked for the rest of the transaction; 404 `case_not_found`.
pub async fn lock(conn: &mut PgConnection, id: &str) -> ApiResult<Item> {
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {ITEM_COLS} FROM {ITEM_FROM} WHERE h.id = $1 FOR UPDATE OF h"
    )))
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or_else(not_found)?;
    let item = item_of(&row)?;
    fresh(conn, item).await
}

/// The open item of a case.
pub async fn open_for_case(conn: &mut PgConnection, case_id: &str) -> ApiResult<Option<Item>> {
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {ITEM_COLS} FROM {ITEM_FROM} \
          WHERE h.case_id = $1 AND h.state NOT IN ('resolved', 'returned_to_agent') FOR UPDATE OF h"
    )))
    .bind(case_id)
    .fetch_optional(&mut *conn)
    .await?;
    match row.as_ref().map(item_of).transpose()? {
        Some(item) => Ok(Some(fresh(conn, item).await?)),
        None => Ok(None),
    }
}

/// The open item of a conversation (newest).
pub async fn open_for_conversation(
    conn: &mut PgConnection,
    conversation_id: &str,
) -> ApiResult<Option<Item>> {
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {ITEM_COLS} FROM {ITEM_FROM} \
          WHERE h.conversation_id = $1 AND h.state NOT IN ('resolved', 'returned_to_agent') \
          ORDER BY h.queued_at DESC LIMIT 1 FOR UPDATE OF h"
    )))
    .bind(conversation_id)
    .fetch_optional(&mut *conn)
    .await?;
    match row.as_ref().map(item_of).transpose()? {
        Some(item) => Ok(Some(fresh(conn, item).await?)),
        None => Ok(None),
    }
}

/// Append one `agent_handover_events` row.
#[allow(clippy::too_many_arguments)]
pub async fn event(
    conn: &mut PgConnection,
    job_id: Uuid,
    item_id: &str,
    actor: &str,
    from: Option<State>,
    to: State,
    assignee: Option<&str>,
    note: &str,
) -> ApiResult<()> {
    sqlx::query(
        "INSERT INTO agent_handover_events \
            (tenant_id, job_id, item_id, at, actor, from_state, to_state, assignee, note) \
         VALUES (app_current_tenant(), $1, $2, now(), $3, $4, $5, $6, $7)",
    )
    .bind(job_id)
    .bind(item_id)
    .bind(actor)
    .bind(from.map(State::as_str))
    .bind(to.as_str())
    .bind(assignee)
    .bind(note)
    .execute(conn)
    .await?;
    Ok(())
}

/// Move `item` to `to` (the caller validated the move: Resolve and Hand back (crate::conversations::ownership) check
/// the state themselves, the internal worker door uses [`transition_error`]): state, `closed_at` and `closed_by`
/// (the actor) on a close, `first_response_at` on the first `waiting_customer`, and the event. A close hands the conversation back
/// to the agent (SW-4) unless another item of it is still open.
pub async fn move_to(
    conn: &mut PgConnection,
    job_id: Uuid,
    item: &Item,
    to: State,
    actor: &str,
    note: &str,
) -> ApiResult<()> {
    sqlx::query(
        "UPDATE agent_handover_items SET state = $2, \
                closed_at = CASE WHEN $2 IN ('resolved', 'returned_to_agent') THEN now() ELSE closed_at END, \
                closed_by = CASE WHEN $2 IN ('resolved', 'returned_to_agent') THEN $3 ELSE closed_by END, \
                first_response_at = CASE WHEN $2 = 'waiting_customer' THEN COALESCE(first_response_at, now()) \
                                         ELSE first_response_at END \
          WHERE id = $1",
    )
    .bind(&item.id)
    .bind(to.as_str())
    .bind(actor)
    .execute(&mut *conn)
    .await?;
    event(
        &mut *conn,
        job_id,
        &item.id,
        actor,
        Some(item.state),
        to,
        None,
        note,
    )
    .await?;
    if to.closed() && !item.state.closed() {
        let other_open: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM agent_handover_items \
                             WHERE conversation_id = $1 AND id <> $2 \
                               AND state NOT IN ('resolved', 'returned_to_agent'))",
        )
        .bind(&item.conversation_id)
        .bind(&item.id)
        .fetch_one(&mut *conn)
        .await?;
        if !other_open {
            ownership::set(conn, &item.conversation_id, &Owner::Agent).await?;
        }
    }
    Ok(())
}

/// (Re)assign `item` to `who` (`applyAssign`): a queued item becomes `assigned`, an open one keeps its state; `who`
/// becomes the conversation's owner ([`ownership::set`]).
pub async fn assign_to(
    conn: &mut PgConnection,
    job_id: Uuid,
    item: &Item,
    who: &str,
    actor: &str,
    note: &str,
) -> ApiResult<State> {
    let to = if item.state == State::Queued {
        State::Assigned
    } else {
        item.state
    };
    sqlx::query("UPDATE agent_handover_items SET state = $2 WHERE id = $1")
        .bind(&item.id)
        .bind(to.as_str())
        .execute(&mut *conn)
        .await?;
    // SW-2: the assignee is the conversation's staff owner
    ownership::set(
        &mut *conn,
        &item.conversation_id,
        &Owner::User(who.to_string()),
    )
    .await?;
    event(
        conn,
        job_id,
        &item.id,
        actor,
        Some(item.state),
        to,
        Some(who),
        note,
    )
    .await?;
    Ok(to)
}

/// The name of `sub` ([`store::MEMBER_NAME_SQL`]) when it may hold items: an enabled member holding `support_agent`
/// or `support_manager` ([`crate::auth::CONVERSATION_WORKERS`]).
pub async fn assignable(conn: &mut PgConnection, sub: &str) -> ApiResult<Option<String>> {
    Ok(sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT {} FROM users \
          WHERE sub = $1 AND disabled_at IS NULL AND {}",
        store::MEMBER_NAME_SQL,
        crate::auth::CONVERSATION_WORKERS_SQL
    )))
    .bind(sub)
    .fetch_optional(conn)
    .await?)
}

/// A new item to queue.
pub struct NewItem<'a> {
    pub job_id: Uuid,
    pub case_id: &'a str,
    pub conv: &'a store::Conv,
    pub reason: &'a str,
    pub trigger: &'a str,
    pub subject: &'a str,
    /// `agent` for the enqueue by the agent, else the staff sub.
    pub actor: &'a str,
    /// The design the item is stamped with ([`store::design_of`], read in the same transaction).
    pub design: &'a CaseDesign,
}

/// The priority a new item is queued with unless its raiser says otherwise ([`ItemExtras::priority`]).
const PRIORITY: &str = "normal";

/// What an agent-raised item carries for the person who takes it (B4 T6a; the columns of 0063). Built server-side by
/// crate::runtime::runner::agent_cases; every field has the column's own default when unset.
#[derive(Clone, Debug, Default)]
pub struct ItemExtras {
    /// `high` / `normal` / `low` (the column's check); `None` = `normal`. Due times are stamped for it.
    pub priority: Option<String>,
    /// JSON array of `{op, outcome, detail}` (web `HandoverAgentStep`).
    pub agent_did: Option<Value>,
    /// JSON array of `{label, value, verified}` (web `HandoverCollected`); `None` = nothing collected.
    pub collected: Option<Value>,
    pub suggested_next: Option<String>,
    /// JSON object (web `HandoverProposal`) or `None`.
    pub proposal_hint: Option<Value>,
}

/// An item queued while the scope is closed (B4 T16a, F-14; crate::handover::hours): its SLA clocks start at
/// `clock_starts_at` (the next open time), and `callback` is the call-back offered to the customer (`None` = a
/// reply after opening).
pub struct OutsideHours {
    pub clock_starts_at: chrono::DateTime<chrono::Utc>,
    pub callback: Option<Value>,
}

/// Queue an item for a case (one open item per case: 409 `already_handed_over`); the case goes to `handover`. The
/// item carries the case's party (else the conversation's) and the due times of the scope's SLA policy for its
/// priority (crate::handover::sla: the live policy for a prod item, the draft for a test item; none = no SLA).
pub async fn create(conn: &mut PgConnection, n: NewItem<'_>) -> ApiResult<String> {
    create_with(conn, n, None).await
}

/// [`create`], raised out of hours when `outside` is set: the item is `outside_hours`, its due times count from
/// `clock_starts_at` instead of now, and its trigger says so.
///
/// The conversation's control lock and owner row are taken first (ownership::guard; a caller that took them already
/// takes nothing new), so queuing locks in the ownership commands' order. A caller that creates the case in the same
/// transaction takes them before `create_case`. After the commit the caller's [`notify_workers`] publishes the
/// conversation's `owner` frame (the queue owns it).
pub async fn create_with(
    conn: &mut PgConnection,
    n: NewItem<'_>,
    outside: Option<&OutsideHours>,
) -> ApiResult<String> {
    create_with_extras(conn, n, outside, &ItemExtras::default()).await
}

/// [`create_with`] carrying `extras` (priority, what the agent did and collected, the suggested first step, the
/// proposal hint) in the same INSERT: the runtime role may insert an item but never update one.
pub async fn create_with_extras(
    conn: &mut PgConnection,
    n: NewItem<'_>,
    outside: Option<&OutsideHours>,
    extras: &ItemExtras,
) -> ApiResult<String> {
    let priority = extras.priority.as_deref().unwrap_or(PRIORITY);
    ownership::guard(&mut *conn, &n.conv.id).await?;
    if open_for_case(conn, n.case_id).await?.is_some() {
        return Err(ApiError::Conflict(
            "already_handed_over",
            "This case is already in the handover queue.".into(),
        ));
    }
    let subject: String = n.subject.trim().chars().take(120).collect();
    let customer = n
        .conv
        .customer_name
        .clone()
        .filter(|c| !c.trim().is_empty())
        .or_else(|| n.conv.customer_id.clone())
        .unwrap_or_default();
    let (release_id, draft_rev) = store::stamp(n.design);
    // B4 T16 (plan R8): due times from the scope's policy — live for a prod item, the draft for a test item
    let due = crate::handover::sla::stamp_minutes(conn, n.job_id, n.design, priority).await?;
    // T16a: out of hours the clocks start at the next open time
    let trigger = match outside {
        Some(_) => format!("{}, outside hours", n.trigger),
        None => n.trigger.to_owned(),
    };
    let id: String = sqlx::query_scalar(
        "INSERT INTO agent_handover_items \
            (tenant_id, job_id, id, case_id, conversation_id, env, case_type, subject, customer, \
             reason, trigger, priority, state, queue, queued_at, release_id, draft_rev, party_id, \
             first_response_due, resolve_due, outside_hours, clock_starts_at, callback, \
             agent_did, collected, suggested_next, proposal_hint) \
         VALUES (app_current_tenant(), $1, agent_next_handover_id(), $2, $3, $4, 'handover', $5, $6, \
                 $7, $8, $11, 'queued', 'default', now(), $9, $10, \
                 COALESCE((SELECT party_id FROM agent_case WHERE case_id = $2), \
                          (SELECT party_id FROM conversation WHERE conversation_id = $3)), \
                 COALESCE($15::timestamptz, now()) + make_interval(mins => $12::int), \
                 COALESCE($15::timestamptz, now()) + make_interval(mins => $13::int), \
                 $14, $15::timestamptz, $16::jsonb, \
                 COALESCE($17::jsonb, '[]'::jsonb), COALESCE($18::jsonb, '[]'::jsonb), $19, $20::jsonb) \
         RETURNING id",
    )
    .bind(n.job_id)
    .bind(n.case_id)
    .bind(&n.conv.id)
    .bind(&n.conv.env)
    .bind(if subject.is_empty() { "Handover" } else { subject.as_str() })
    .bind(customer)
    .bind(n.reason)
    .bind(trigger.clone())
    .bind(release_id)
    .bind(draft_rev)
    .bind(priority)
    .bind(due.map(|t| t.first_response_min as i32))
    .bind(due.map(|t| t.resolve_min as i32))
    .bind(outside.is_some())
    .bind(outside.map(|o| o.clock_starts_at))
    .bind(outside.and_then(|o| o.callback.clone()))
    .bind(extras.agent_did.clone())
    .bind(extras.collected.clone())
    .bind(extras.suggested_next.clone())
    .bind(extras.proposal_hint.clone())
    .fetch_one(&mut *conn)
    .await?;
    event(
        conn,
        n.job_id,
        &id,
        n.actor,
        None,
        State::Queued,
        None,
        &trigger,
    )
    .await?;
    store::set_case_status(&mut *conn, n.case_id, "handover", Some(n.reason)).await?;
    // SW-1: an open item nobody has taken: the queue owns the conversation
    ownership::set(conn, &n.conv.id, &Owner::Queue).await?;
    // staff own the conversation now, so its unfinished MCP tasks too: answered in the Tasks inbox, not in the chat
    crate::mcp_client::store::escalate_conversation(conn, &n.conv.id, "handover").await?;
    Ok(id)
}

/// Names of `subs` ([`store::member_name`]: display name, else email, else the remembered staff name, else the sub).
async fn names(
    conn: &mut PgConnection,
    subs: &[String],
) -> ApiResult<std::collections::HashMap<String, String>> {
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT s.sub, coalesce((SELECT {} FROM users u WHERE u.sub = s.sub), agent_actor_name(s.sub), s.sub) AS name \
           FROM unnest($1::text[]) AS s(sub)",
        store::MEMBER_NAME_SQL
    )))
    .bind(subs)
    .fetch_all(conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| (r.get("sub"), r.get("name")))
        .collect())
}

/// The answered rating of case `case_id` as `{rating, recorded_at}`, else `null` (asked, expired or never asked).
pub async fn csat_of_case(conn: &mut PgConnection, case_id: &str) -> ApiResult<Value> {
    let row = sqlx::query(
        "SELECT rating, answered_at FROM case_csat WHERE case_id = $1 AND answered_at IS NOT NULL",
    )
    .bind(case_id)
    .fetch_optional(conn)
    .await?;
    Ok(row.map_or(Value::Null, |r| csat_json(&r)))
}

/// The newest answered rating of conversation `conversation_id` as `{rating, recorded_at}`, else `null`.
pub async fn csat_of_conversation(
    conn: &mut PgConnection,
    conversation_id: &str,
) -> ApiResult<Value> {
    let row = sqlx::query(
        "SELECT rating, answered_at FROM case_csat \
          WHERE conversation_id = $1 AND answered_at IS NOT NULL ORDER BY answered_at DESC LIMIT 1",
    )
    .bind(conversation_id)
    .fetch_optional(conn)
    .await?;
    Ok(row.map_or(Value::Null, |r| csat_json(&r)))
}

fn csat_json(r: &sqlx::postgres::PgRow) -> Value {
    json!({
        "rating": r.get::<Option<i16>, _>("rating"),
        "recorded_at": ts(r, "answered_at"),
    })
}

/// The `author` of the stored handover notice (an `agent` line, as the CSAT prompt is `csat`).
pub const NOTICE_AUTHOR: &str = "handover_notice";

/// Store the handover notice the edge sends the customer when an inbound message is queued with `notice` (B13: the
/// transcript shows what the customer saw). The text is the edge's own (`outbound_text` of `HandoverNotice`).
pub async fn store_notice(
    conn: &mut PgConnection,
    job_id: Uuid,
    conversation_id: &str,
    case_id: &str,
) -> ApiResult<()> {
    let text = crate::channels::connector::outbound_text(
        &crate::channels::connector::Outbound::HandoverNotice { eta_minutes: None },
    )
    .unwrap_or_default();
    store::append(
        conn,
        store::NewMessage {
            job_id,
            conversation_id,
            role: "agent",
            author: NOTICE_AUTHOR,
            text: &text,
            case_id: Some(case_id),
            external_id: None,
        },
    )
    .await?;
    Ok(())
}

/// Did the customer message `external_id` queue `item` (B7 R6: a resent accept whose first accept timed out at the
/// edge)? The item is still queued; the message is of the item's case and the conversation's newest customer
/// message, and the case's handover notice ([`store_notice`]) follows it. A resend of any other message is not. Nor
/// is one once an edge reported the notice delivered (`notice_delivered_at`, `POST
/// /api/internal/channel/handover/notice-delivered`): the edge's dedupe is per process, so without that ack a resend
/// landing on another pod, or after the edge already replied, would send the notice twice.
pub async fn queued_by(conn: &mut PgConnection, item: &Item, external_id: &str) -> ApiResult<bool> {
    if item.state != State::Queued {
        return Ok(false);
    }
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM conversation_message m \
          WHERE m.conversation_id = $1 AND m.external_id = $2 AND m.role = 'customer' AND m.case_id = $3 \
            AND NOT EXISTS (SELECT 1 FROM agent_handover_items i \
                             WHERE i.id = $5 AND i.notice_delivered_at IS NOT NULL) \
            AND NOT EXISTS (SELECT 1 FROM conversation_message n \
                             WHERE n.conversation_id = m.conversation_id AND n.role = 'customer' \
                               AND (n.at_ms, n.message_id) > (m.at_ms, m.message_id)) \
            AND EXISTS (SELECT 1 FROM conversation_message n \
                         WHERE n.conversation_id = m.conversation_id AND n.role = 'agent' AND n.author = $4 \
                           AND n.case_id = $3 AND (n.at_ms, n.message_id) > (m.at_ms, m.message_id)))",
    )
    .bind(&item.conversation_id)
    .bind(external_id)
    .bind(&item.case_id)
    .bind(NOTICE_AUTHOR)
    .bind(&item.id)
    .fetch_one(conn)
    .await?)
}

/// The edge delivered `item_id`'s handover notice to the customer: stamp it once (idempotent; an unknown item, or one
/// already stamped, changes nothing). Runs in the caller's job scope (RLS), so ids of other jobs never match.
pub async fn mark_notice_delivered(
    conn: &mut PgConnection,
    conversation_id: &str,
    item_id: &str,
) -> ApiResult<()> {
    sqlx::query(
        "UPDATE agent_handover_items SET notice_delivered_at = now() \
          WHERE id = $1 AND conversation_id = $2 AND notice_delivered_at IS NULL",
    )
    .bind(item_id)
    .bind(conversation_id)
    .execute(conn)
    .await?;
    Ok(())
}

fn ts(r: &sqlx::postgres::PgRow, col: &str) -> Value {
    r.get::<Option<chrono::DateTime<chrono::Utc>>, _>(col)
        .map(iso)
        .map_or(Value::Null, Value::from)
}

/// Read from `agent_handover_items_owned` (0200): `channel` and `assignee` are derived there.
const VIEW_COLS: &str = "id, case_id, conversation_id, case_type, subject, customer, order_id, channel, reason, \
     trigger, priority, state, queue, queued_at, first_response_at, closed_at, first_response_due, resolve_due, \
     assignee, release_id::text AS release_id, confidence::float8 AS confidence, \
     COALESCE(agent_did, '[]'::jsonb) AS agent_did, CASE WHEN jsonb_typeof(collected) = 'array' THEN collected ELSE '[]'::jsonb END AS collected, \
     COALESCE(suggested_next, '') AS suggested_next, proposal_hint, party_id, outside_hours, callback, \
     clock_starts_at";

/// One item as the web's `HandoverItem`.
async fn view_of(conn: &mut PgConnection, r: &sqlx::postgres::PgRow) -> ApiResult<Value> {
    let id: String = r.get("id");
    let conversation_id: String = r.get("conversation_id");
    let assignee: Option<String> = r.get("assignee");
    let evs = sqlx::query(
        "SELECT at, actor, from_state, to_state, assignee, COALESCE(note, '') AS note \
           FROM agent_handover_events WHERE item_id = $1 ORDER BY seq",
    )
    .bind(&id)
    .fetch_all(&mut *conn)
    .await?;
    let mut subs: Vec<String> = evs
        .iter()
        .map(|e| e.get::<String, _>("actor"))
        .filter(|a| a != "agent")
        .collect();
    subs.extend(assignee.clone());
    let fb = sqlx::query(
        "SELECT verdict, COALESCE(note, '') AS note, by_sub, at, proposal, drafted_at \
           FROM agent_handover_feedback WHERE item_id = $1",
    )
    .bind(&id)
    .fetch_optional(&mut *conn)
    .await?;
    if let Some(f) = &fb {
        subs.push(f.get("by_sub"));
    }
    let nm = names(conn, &subs).await?;
    let name = |s: &str| {
        if s == "agent" {
            "Agent".to_string()
        } else {
            nm.get(s).cloned().unwrap_or_else(|| s.to_string())
        }
    };
    let history: Vec<Value> = evs
        .iter()
        .map(|e| {
            let actor: String = e.get("actor");
            let mut v = json!({
                "at": ts(e, "at"),
                "actor": actor,
                "actor_name": name(&actor),
                "from": e.get::<Option<String>, _>("from_state"),
                "to": e.get::<String, _>("to_state"),
                "note": e.get::<String, _>("note"),
            });
            if let Some(a) = e.get::<Option<String>, _>("assignee") {
                v["assignee"] = json!(a);
            }
            v
        })
        .collect();
    // SW T6: typed `MessageView`s (a `system` row is `kind: "system"` with its `event`, never a bubble), each also
    // with the B12 `HandoverMessage` fields `from` and `author`
    let messages: Vec<Value> = store::last_views(conn, &conversation_id, 100)
        .await?
        .into_iter()
        .map(|(v, author, _)| {
            let mut m = json!(v);
            m["from"] = json!(v.kind);
            m["author"] = json!(author);
            m
        })
        .collect();
    let owner = ownership::view(conn, &conversation_id).await?;
    let party = crate::party::brief(conn, r.get("party_id")).await?;
    let csat = match r.get::<Option<String>, _>("case_id") {
        Some(c) => csat_of_case(conn, &c).await?,
        None => Value::Null,
    };
    let feedback = fb.map(|f| {
        let by: String = f.get("by_sub");
        json!({
            "verdict": f.get::<String, _>("verdict"),
            "note": f.get::<String, _>("note"),
            "by": by,
            "by_name": name(&by),
            "at": ts(&f, "at"),
            "proposal": f.get::<Option<Value>, _>("proposal"),
            "drafted_at": ts(&f, "drafted_at"),
        })
    });
    Ok(json!({
        "id": id,
        "conversation_id": conversation_id,
        "owner": owner,
        "case_type": r.get::<String, _>("case_type"),
        "subject": r.get::<String, _>("subject"),
        "customer": r.get::<Option<String>, _>("customer").unwrap_or_default(),
        "order_id": r.get::<Option<String>, _>("order_id"),
        "channel": r.get::<String, _>("channel"),
        "reason": r.get::<String, _>("reason"),
        "trigger": r.get::<Option<String>, _>("trigger").unwrap_or_default(),
        "priority": r.get::<String, _>("priority"),
        "state": r.get::<String, _>("state"),
        "queued_at": ts(r, "queued_at"),
        "first_response_at": ts(r, "first_response_at"),
        "closed_at": ts(r, "closed_at"),
        "first_response_due": ts(r, "first_response_due"),
        "resolve_due": ts(r, "resolve_due"),
        "assignee_name": assignee.as_deref().map(name),
        "assignee": assignee,
        "history": history,
        "messages": messages,
        "case_ref": r.get::<Option<String>, _>("case_id"),
        "release_id": r.get::<Option<String>, _>("release_id"),
        "queue": r.get::<Option<String>, _>("queue").unwrap_or_else(|| "default".into()),
        "confidence": r.get::<Option<f64>, _>("confidence"),
        "agent_did": r.get::<Value, _>("agent_did"),
        "collected": r.get::<Value, _>("collected"),
        "suggested_next": r.get::<String, _>("suggested_next"),
        "proposal_hint": r.get::<Option<Value>, _>("proposal_hint"),
        // T16a (F-14): raised out of hours; its SLA clocks start at `clock_starts_at`
        "outside_hours": r.get::<bool, _>("outside_hours"),
        "callback": r.get::<Option<Value>, _>("callback"),
        "clock_starts_at": ts(r, "clock_starts_at"),
        "feedback": feedback,
        "party": party,
        "csat": csat,
    }))
}

/// One item as `HandoverItem`; 404 `case_not_found`.
pub async fn view(conn: &mut PgConnection, id: &str) -> ApiResult<Value> {
    let r = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {VIEW_COLS} FROM agent_handover_items_owned WHERE id = $1"
    )))
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or_else(not_found)?;
    view_of(conn, &r).await
}

/// One handover item of a conversation as the transcript splits it (SW T6, the web's `Episode`): the plain-language
/// reason ([`crate::support::names::reason_label`]), when it was queued and closed, and how it closed (`resolution`:
/// a Resolve's outcome, else the closed state, e.g. `returned_to_agent`; `null` while open). The reason code, the
/// trigger (which may name decision codes) and the state are only in `details`.
#[derive(Clone, Debug, Serialize)]
pub struct Episode {
    pub item_id: String,
    pub case_id: Option<String>,
    pub reason_label: &'static str,
    pub queued_at: String,
    pub closed_at: Option<String>,
    pub resolution: Option<String>,
    pub details: Value,
}

/// The episodes of `conversation_id`: every handover item of it, oldest first (`queued_at`, then id).
pub async fn episodes(conn: &mut PgConnection, conversation_id: &str) -> ApiResult<Vec<Episode>> {
    let rows = sqlx::query(
        "SELECT id, case_id, reason, trigger, state, resolution, queued_at, closed_at \
           FROM agent_handover_items WHERE conversation_id = $1 ORDER BY queued_at, id",
    )
    .bind(conversation_id)
    .fetch_all(conn)
    .await?;
    Ok(rows
        .iter()
        .map(|r| {
            let reason: String = r.get("reason");
            let state: String = r.get("state");
            let closed_at: Option<chrono::DateTime<chrono::Utc>> = r.get("closed_at");
            let resolution = closed_at.and(
                r.get::<Option<String>, _>("resolution")
                    .or_else(|| Some(state.clone())),
            );
            Episode {
                item_id: r.get("id"),
                case_id: r.get("case_id"),
                reason_label: crate::support::names::reason_label(&reason),
                queued_at: iso(r.get("queued_at")),
                closed_at: closed_at.map(iso),
                resolution,
                details: json!({
                    "reason": reason,
                    "trigger": r.get::<Option<String>, _>("trigger"),
                    "state": state,
                }),
            }
        })
        .collect())
}

/// The most items one queue read returns.
pub const QUEUE_LIMIT: i64 = 200;

/// `handoverQueue`: the job's items (`state` = a state, `open` or none; `assignee` = a sub or `none`), open first,
/// newest first; the assignees; and the server's `now`.
pub async fn queue(
    conn: &mut PgConnection,
    state: Option<&str>,
    assignee: Option<&str>,
    only_for: Option<&str>,
) -> ApiResult<Value> {
    let state = match state.map(str::trim).filter(|s| !s.is_empty()) {
        None | Some("all") => None,
        Some("open") => Some("open"),
        Some(s) => Some(State::parse(s).map(State::as_str).ok_or_else(|| {
            ApiError::Unprocessable("invalid_query", "state is not a handover state".into())
        })?),
    };
    let assignee = assignee.map(str::trim).filter(|s| !s.is_empty());
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {VIEW_COLS} FROM agent_handover_items_owned o \
          WHERE ($1::text IS NULL \
                 OR ($1 = 'open' AND state NOT IN ('resolved', 'returned_to_agent')) \
                 OR state = $1) \
            AND ($2::text IS NULL OR ($2 = 'none' AND assignee IS NULL) OR assignee = $2) \
            AND ($4::text IS NULL OR {team} OR o.assignee = $4) \
          ORDER BY (state IN ('resolved', 'returned_to_agent')), queued_at DESC, id \
          LIMIT $3",
        team = crate::support::job_staff::team_open_sql("o.tenant_id", "o.job_id", "$4")
    )))
    .bind(state)
    .bind(assignee)
    .bind(QUEUE_LIMIT)
    .bind(only_for)
    .fetch_all(&mut *conn)
    .await?;
    let mut items = Vec::with_capacity(rows.len());
    for r in &rows {
        items.push(view_of(conn, r).await?);
    }
    let assignees = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT sub, {} AS name, roles FROM users \
          WHERE disabled_at IS NULL AND {} ORDER BY sub",
        store::MEMBER_NAME_SQL,
        crate::auth::CONVERSATION_WORKERS_SQL
    )))
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .map(|r| {
        json!({
            "name": r.get::<String, _>("name"),
            "sub": r.get::<String, _>("sub"),
            "roles": r.get::<Vec<String>, _>("roles"),
        })
    })
    .collect::<Vec<_>>();
    let now: chrono::DateTime<chrono::Utc> = sqlx::query_scalar("SELECT now()")
        .fetch_one(&mut *conn)
        .await?;
    Ok(json!({"items": items, "next": null, "assignees": assignees, "now": iso(now)}))
}

/// The unassigned queued item to take next: an item whose SLA clock has not started yet (raised out of hours, T16a)
/// after every started one; then breached first, then priority, then first-response due (none last), then the
/// longest waiting. `SKIP LOCKED`: two concurrent takers get different items.
pub async fn next_unassigned(conn: &mut PgConnection) -> ApiResult<Option<Item>> {
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {ITEM_COLS} FROM {ITEM_FROM} \
          WHERE h.state = 'queued' AND o.assignee IS NULL \
          ORDER BY (h.clock_starts_at IS NOT NULL AND h.clock_starts_at > now()), \
                   (h.first_response_due IS NOT NULL AND h.first_response_due < now()) DESC, \
                   CASE h.priority WHEN 'urgent' THEN 0 WHEN 'high' THEN 1 WHEN 'normal' THEN 2 ELSE 3 END, \
                   h.first_response_due NULLS LAST, h.queued_at, h.id \
          LIMIT 1 FOR UPDATE OF h SKIP LOCKED"
    )))
    .fetch_optional(&mut *conn)
    .await?;
    match row.as_ref().map(item_of).transpose()? {
        Some(item) => Ok(Some(fresh(conn, item).await?)),
        None => Ok(None),
    }
}

/// After an item is queued (and committed): publish the conversation's `owner` frame (ownership::publish_queued),
/// tell the job's enabled worker bindings and store the threads they open. Failures are logged (ids and codes only);
/// the item stays queued either way.
pub async fn notify_workers(st: &AppState, tenant: Uuid, job_id: Uuid, item_id: &str) {
    // SW T5: every path that queues an item calls this after its commit: the queue owns the conversation now
    ownership::publish_queued(st, tenant, job_id, item_id).await;
    if let Err(e) = try_notify(st, tenant, job_id, item_id).await {
        tracing::warn!(
            item = item_id,
            code = e.code(),
            "handover_notify_workers_failed"
        );
    }
    // support routing: after the worker channels have the queued item (a worker thread's reply is a Take), hand it to
    // an online member when the tenant's routing is on; an item a Take / Assign created is already assigned
    crate::support::routing::auto_assign_item(st, tenant, job_id, item_id).await;
}

async fn try_notify(st: &AppState, tenant: Uuid, job_id: Uuid, item_id: &str) -> ApiResult<()> {
    let Some(edge) = st.edge.clone() else {
        return Ok(());
    };
    let mut tx = store::begin_job(&st.db, tenant, job_id).await?;
    let targets: Vec<WorkerTarget> = sqlx::query(
        "SELECT id, kind, settings FROM connector_bindings \
          WHERE job_id = $1 AND side = 'worker' AND enabled AND verified_at IS NOT NULL \
          ORDER BY created_at",
    )
    .bind(job_id)
    .fetch_all(&mut *tx)
    .await?
    .into_iter()
    .map(|r| WorkerTarget {
        binding_id: r.get("id"),
        kind: r.get("kind"),
        settings: r.get("settings"),
    })
    .collect();
    if targets.is_empty() {
        tx.commit().await?;
        return Ok(());
    }
    // the customer's email (R9: a helpdesk ticket's requester): the item's party's `email` identity, verified first
    let r = sqlx::query(
        "SELECT h.conversation_id, h.subject, h.customer, h.channel, h.reason, h.priority, \
                (SELECT pi.subject FROM party_identity pi \
                  WHERE pi.party_id = h.party_id AND pi.channel = 'email' \
                  ORDER BY pi.verified DESC, pi.last_seen_at DESC LIMIT 1) AS customer_email \
           FROM agent_handover_items_owned h WHERE h.id = $1",
    )
    .bind(item_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(not_found)?;
    let conversation_id: String = r.get("conversation_id");
    // SW-11: the excerpt is `last_shared`: a staff note is not in it
    let excerpt: Vec<ExcerptLine> = store::last_shared(&mut tx, &conversation_id, 6)
        .await?
        .into_iter()
        .map(|m| ExcerptLine {
            role: m.role,
            text: m.text,
        })
        .collect();
    let req = NotifyWorkersReq {
        tenant,
        job_id,
        item: WorkerItem {
            id: item_id.to_string(),
            subject: r.get("subject"),
            customer: r.get::<Option<String>, _>("customer").unwrap_or_default(),
            channel: r.get("channel"),
            reason: r.get("reason"),
            priority: r.get("priority"),
            excerpt,
            customer_email: r.get("customer_email"),
        },
        targets: targets.clone(),
    };
    tx.commit().await?;
    let results = edge.notify_workers(&req).await.map_err(|e| {
        ApiError::BadGateway(e.code, "the worker channels were not notified".into())
    })?;
    let mut tx = store::begin_job(&st.db, tenant, job_id).await?;
    for res in results {
        let Some(t) = targets.iter().find(|t| t.binding_id == res.binding_id) else {
            continue;
        };
        match (res.ok, res.thread) {
            (true, Some(mut thread)) => {
                // SW-11: the binding that opened the thread, for an internal note posted to it later
                thread
                    .route
                    .insert("binding_id".into(), t.binding_id.to_string());
                store::save_worker_ref(
                    &mut tx,
                    job_id,
                    &conversation_id,
                    item_id,
                    &t.kind,
                    &thread,
                )
                .await?;
            }
            _ => {
                tracing::warn!(item = item_id, binding = %res.binding_id, "handover_worker_target_failed")
            }
        }
    }
    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transitions_follow_j21() {
        use State::*;
        assert_eq!(transition_error(Resolved, Assigned), Some("closed"));
        assert_eq!(transition_error(Queued, Resolved), Some("assign_first"));
        assert_eq!(transition_error(Queued, Queued), Some("assign_first"));
        assert_eq!(transition_error(Queued, ReturnedToAgent), None);
        assert_eq!(transition_error(Assigned, Assigned), Some("same_state"));
        assert_eq!(
            transition_error(Assigned, Queued),
            Some("invalid_transition")
        );
        assert_eq!(transition_error(Assigned, WaitingCustomer), None);
        assert_eq!(transition_error(WaitingCustomer, Assigned), None);
        assert_eq!(transition_error(WaitingCustomer, Resolved), None);
        for s in [
            "queued",
            "assigned",
            "waiting_customer",
            "resolved",
            "returned_to_agent",
        ] {
            assert_eq!(State::parse(s).unwrap().as_str(), s);
        }
        assert!(State::parse("nope").is_none());
    }
}
