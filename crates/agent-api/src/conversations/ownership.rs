//! The one owner record of a conversation (Staff Workspace SW-1, migration 0190): `conversation_control`. Who answers
//! the customer is exactly one of the agent, the queue (an open handover item nobody has taken) or one staff member.
//! A handover item's assignee is derived from it (view `agent_handover_items_owned`, SW-2).
//!
//! [`set`] is the only writer. Every conversation has a row (written when it is created; 0190 backfilled the rest);
//! [`current`] still reads a conversation without one as agent-owned, and [`lock`] writes the missing row first, so
//! a command can always lock it.
//!
//! A closed item's derived `assignee` (the view) is its `closed_by`, the staff member who closed it
//! (`NULLIF(closed_by, 'agent')`), not the last person who held it.
//!
//! The ownership commands (SW T2/T4) are [`take`], [`assign`], [`hand_back`] and [`resolve`]; the worker-thread door
//! (crate::conversations::internal: Frappe Helpdesk) runs the same Take and Resolve as [`take_scoped`]
//! and [`resolve_scoped`] (SW-5, T5), in a binding's scope for the staff member it mapped ([`By`]). Each runs in one
//! transaction and locks in one order: the conversation's control lock (`store::control_key`, which the runtime also takes first),
//! then the owner row ([`lock`]), then the open item. After the commit each publishes an `owner` frame
//! `{conversation_id, owner, name, since, item_id}` and a `handover` frame for every item it changed.
//!
//! Every other path that queues an item or changes the owner takes the same two locks first ([`guard`]: before
//! `create_case` / `agent_case_seq`, the case row or the item), and `handover::create_with` takes them itself; after
//! its commit the path's `handover::notify_workers` publishes the `owner` frame ([`publish_queued`]).
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{PgConnection, Row};

use super::edge_link::DeliverKind;
use super::handover::{self, State as HoState};
use super::staff::{MsgKind, Reply};
use super::{csat, iso, staff, store};
use crate::audit::{self, Actor, Event};
use crate::error::{ApiError, ApiResult};
use crate::handover::feedback::{self, Verdict};
use crate::interaction::Scope;
use crate::state::AppState;
use crate::tenancy::JobCtx;

/// Who answers the customer. Wire: `agent` | `queue` | `user:<sub>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Owner {
    Agent,
    Queue,
    User(String),
}

impl Owner {
    /// The wire form: `agent`, `queue` or `user:<sub>`.
    pub fn wire(&self) -> String {
        match self {
            Owner::Agent => "agent".to_string(),
            Owner::Queue => "queue".to_string(),
            Owner::User(sub) => format!("user:{sub}"),
        }
    }

    /// The owner of a wire form ([`Owner::wire`]); `None` for anything else (an empty sub included).
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "agent" => Some(Owner::Agent),
            "queue" => Some(Owner::Queue),
            _ => s
                .strip_prefix("user:")
                .filter(|sub| !sub.trim().is_empty())
                .map(|sub| Owner::User(sub.to_string())),
        }
    }

    /// The `conversation_control.mode` it is stored as.
    pub fn mode(&self) -> &'static str {
        match self {
            Owner::Agent => "agent",
            Owner::Queue => "queue",
            Owner::User(_) => "staff",
        }
    }

    /// The `conversation_control.by_sub` it is stored as (set exactly for a staff owner).
    pub fn by_sub(&self) -> Option<&str> {
        match self {
            Owner::User(sub) => Some(sub.as_str()),
            _ => None,
        }
    }

    fn of_row(mode: &str, by_sub: Option<String>) -> ApiResult<Self> {
        match (mode, by_sub) {
            ("agent", _) => Ok(Owner::Agent),
            ("queue", _) => Ok(Owner::Queue),
            ("staff", Some(sub)) => Ok(Owner::User(sub)),
            _ => Err(ApiError::Internal(anyhow::anyhow!(
                "unknown conversation owner"
            ))),
        }
    }
}

/// The owner of a conversation, since when, and (a staff owner) the owner's display name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnerState {
    pub owner: Owner,
    pub since: DateTime<Utc>,
    pub name: Option<String>,
}

/// The agent stays silent while anyone but the agent owns the conversation (SW-1).
pub fn silenced(o: &Owner) -> bool {
    *o != Owner::Agent
}

/// The owner of `conversation_id` with the owner's name.
pub async fn current(conn: &mut PgConnection, conversation_id: &str) -> ApiResult<OwnerState> {
    let s = read(&mut *conn, conversation_id).await?;
    named(conn, s).await
}

/// The owner of `conversation_id` without the name lookup: what the runtime reads for the silence rule
/// ([`silenced`]; the runtime role reads neither `users` nor the remembered staff names).
pub(crate) async fn read(conn: &mut PgConnection, conversation_id: &str) -> ApiResult<OwnerState> {
    let row = sqlx::query(
        "SELECT COALESCE(cc.mode, 'agent') AS mode, cc.by_sub, COALESCE(cc.since, v.started_at) AS since \
           FROM conversation v \
           LEFT JOIN conversation_control cc \
             ON cc.tenant_id = v.tenant_id AND cc.job_id = v.job_id AND cc.conversation_id = v.conversation_id \
          WHERE v.conversation_id = $1",
    )
    .bind(conversation_id)
    .fetch_optional(conn)
    .await?;
    match row {
        Some(r) => Ok(OwnerState {
            owner: Owner::of_row(&r.get::<String, _>("mode"), r.get("by_sub"))?,
            since: r.get("since"),
            name: None,
        }),
        // no such conversation: nobody but the agent has ever owned it
        None => Ok(OwnerState {
            owner: Owner::Agent,
            since: Utc::now(),
            name: None,
        }),
    }
}

/// The owner row of `conversation_id`, locked (`FOR UPDATE`) for the rest of the transaction; a conversation
/// without one gets its agent row first. 404 `conversation_not_found`.
pub async fn lock(conn: &mut PgConnection, conversation_id: &str) -> ApiResult<OwnerState> {
    let s = lock_row(&mut *conn, conversation_id)
        .await?
        .ok_or_else(|| {
            ApiError::Missing(
                "conversation_not_found",
                "No such conversation in this job.".into(),
            )
        })?;
    named(conn, s).await
}

/// The conversation's control lock (`store::control_key`) and then its owner row ([`lock`], without the name lookup:
/// the runtime role reads no names), for the rest of the transaction: the first two locks of every path that queues
/// a handover item or changes the owner, taken before anything else is locked or written (the case counter of
/// `create_case`, a case row, an item), so every such path orders control lock → owner row → rows like the commands
/// do. Taken twice in one transaction it changes nothing. `None` when the conversation does not exist (nothing to
/// own; the control lock is still held).
pub(crate) async fn guard(
    conn: &mut PgConnection,
    conversation_id: &str,
) -> ApiResult<Option<OwnerState>> {
    store::lock(&mut *conn, &store::control_key(conversation_id)).await?;
    lock_row(conn, conversation_id).await
}

/// The owner row of `conversation_id` `FOR UPDATE` (written first when missing), without the name; `None` when the
/// conversation does not exist.
async fn lock_row(conn: &mut PgConnection, conversation_id: &str) -> ApiResult<Option<OwnerState>> {
    sqlx::query(
        "INSERT INTO conversation_control (tenant_id, job_id, conversation_id, mode, by_sub, since) \
         SELECT v.tenant_id, v.job_id, v.conversation_id, 'agent', NULL, v.started_at \
           FROM conversation v WHERE v.conversation_id = $1 \
         ON CONFLICT (tenant_id, job_id, conversation_id) DO NOTHING",
    )
    .bind(conversation_id)
    .execute(&mut *conn)
    .await?;
    let row = sqlx::query(
        "SELECT mode, by_sub, since FROM conversation_control WHERE conversation_id = $1 FOR UPDATE",
    )
    .bind(conversation_id)
    .fetch_optional(&mut *conn)
    .await?;
    row.map(|row| -> ApiResult<OwnerState> {
        Ok(OwnerState {
            owner: Owner::of_row(&row.get::<String, _>("mode"), row.get("by_sub"))?,
            since: row.get("since"),
            name: None,
        })
    })
    .transpose()
}

/// The owner upsert of [`set`] (`$1` conversation id, `$2` mode, `$3` by_sub). Public so the runtime-role test
/// (tests/b4/runtime_role.rs) runs exactly this text as `agent_runtime`, its DO UPDATE path included.
pub const SET_SQL: &str = "INSERT INTO conversation_control (tenant_id, job_id, conversation_id, mode, by_sub, since) \
     SELECT v.tenant_id, v.job_id, v.conversation_id, $2, $3, now() \
       FROM conversation v WHERE v.conversation_id = $1 \
     ON CONFLICT (tenant_id, job_id, conversation_id) DO UPDATE \
        SET mode = EXCLUDED.mode, by_sub = EXCLUDED.by_sub, \
            since = CASE WHEN conversation_control.mode = EXCLUDED.mode \
                          AND conversation_control.by_sub IS NOT DISTINCT FROM EXCLUDED.by_sub \
                         THEN conversation_control.since ELSE now() END";

/// Make `to` the owner of `conversation_id` (THE writer of `conversation_control`). `since` moves only when the
/// owner changes. A conversation that does not exist has no owner row to write (an item of a case whose
/// conversation row was never written, e.g. a fixture): nothing is written, and a warning names the ids.
pub(crate) async fn set(
    conn: &mut PgConnection,
    conversation_id: &str,
    to: &Owner,
) -> ApiResult<()> {
    let done = sqlx::query(SET_SQL)
        .bind(conversation_id)
        .bind(to.mode())
        .bind(to.by_sub())
        .execute(conn)
        .await?;
    if done.rows_affected() == 0 {
        tracing::warn!(
            conversation = %conversation_id,
            mode = to.mode(),
            "ownership_set_no_conversation"
        );
    }
    Ok(())
}

// ---- commands (SW T2) -------------------------------------------------------------------------------------------

/// `OwnerView`: the owner on the wire (`agent` | `queue` | `user:<sub>`), the staff owner's name, since when, and the
/// conversation's open handover item.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct OwnerView {
    pub owner: String,
    pub name: Option<String>,
    pub since: String,
    pub item_id: Option<String>,
    /// Routing (0204) assigned the open item and the owner is still that assignee: no person took or assigned it.
    /// The one source of the staff workspace's "Auto" chip (inbox row and conversation header).
    pub auto_assigned: bool,
}

impl OwnerView {
    pub fn of(s: &OwnerState, item_id: Option<String>) -> Self {
        OwnerView {
            owner: s.owner.wire(),
            name: s.name.clone(),
            since: iso(s.since),
            item_id,
            auto_assigned: false,
        }
    }

    /// Set [`OwnerView::auto_assigned`] from the open item's `auto_assigned_at` stamp (`stamped`): true only while a
    /// person owns the conversation.
    pub fn with_auto(mut self, stamped: bool) -> Self {
        self.auto_assigned = stamped && self.owner.starts_with("user:");
        self
    }

    fn json(&self) -> Value {
        json!({"owner": self.owner, "name": self.name, "since": self.since, "item_id": self.item_id,
               "auto_assigned": self.auto_assigned})
    }

    /// The `owner` frame of conversation `conv`.
    fn frame(&self, conv: &str) -> Value {
        json!({"conversation_id": conv, "owner": self.owner, "name": self.name, "since": self.since,
               "item_id": self.item_id, "auto_assigned": self.auto_assigned})
    }
}

/// Who runs a command, in which job: a member route's caller ([`JobCtx`]), or the worker-thread door's binding scope
/// and the staff member it mapped by email (crate::conversations::internal).
#[derive(Clone, Debug)]
pub struct By {
    pub scope: Scope,
    /// The audit actor.
    pub actor: Actor,
    /// The member running it: the initiator of a hand-over the command records.
    pub sub: String,
}

impl From<&JobCtx> for By {
    fn from(j: &JobCtx) -> Self {
        By {
            scope: Scope::from(j),
            actor: j.t.actor(),
            sub: j.t.caller.sub.clone(),
        }
    }
}

/// The `system` message author of a hand-back (its text carries the note to the agent).
pub const HAND_BACK_AUTHOR: &str = "hand_back";

/// The `system` message author of a Take / Assign ("… took this conversation", "… assigned this conversation to …").
pub const OWNER_AUTHOR: &str = "owner";

/// One `system` line of an ownership command in the transcript (never delivered to the customer).
async fn system_row(
    conn: &mut PgConnection,
    job_id: uuid::Uuid,
    conv: &str,
    author: &str,
    text: &str,
    case_id: Option<&str>,
) -> ApiResult<()> {
    store::append(
        conn,
        store::NewMessage {
            job_id,
            conversation_id: conv,
            role: "system",
            author,
            text,
            case_id,
            external_id: None,
        },
    )
    .await?;
    Ok(())
}

/// The `expected_owner` of a command: 422 `invalid_owner` unless it is a wire owner.
pub fn expected(s: &str) -> ApiResult<Owner> {
    store::require_clean([s])?;
    Owner::parse(s.trim()).ok_or_else(|| {
        ApiError::Unprocessable(
            "invalid_owner",
            "expected_owner is agent, queue or user:<sub>.".into(),
        )
    })
}

/// The [`OwnerView`] of `conversation_id` now (not locked): what every read projection shows (SW T6: the console
/// row, the handover queue and item, the case row and detail, the transcript), so they never disagree. A
/// conversation that does not exist reads as agent-owned.
pub async fn view(conn: &mut PgConnection, conversation_id: &str) -> ApiResult<OwnerView> {
    let s = current(&mut *conn, conversation_id).await?;
    let (item, stamped) = open_item(conn, conversation_id).await?;
    Ok(OwnerView::of(&s, item).with_auto(stamped))
}

/// The newest open handover item of `conversation_id` and whether routing stamped it (`auto_assigned_at`).
async fn open_item(
    conn: &mut PgConnection,
    conversation_id: &str,
) -> ApiResult<(Option<String>, bool)> {
    let row: Option<(String, bool)> = sqlx::query_as(
        "SELECT id, auto_assigned_at IS NOT NULL FROM agent_handover_items \
          WHERE conversation_id = $1 AND state NOT IN ('resolved', 'returned_to_agent') \
          ORDER BY queued_at DESC LIMIT 1",
    )
    .bind(conversation_id)
    .fetch_optional(conn)
    .await?;
    Ok(match row {
        Some((id, stamped)) => (Some(id), stamped),
        None => (None, false),
    })
}

/// The newest open handover item of `conversation_id` (not locked).
async fn open_item_id(conn: &mut PgConnection, conversation_id: &str) -> ApiResult<Option<String>> {
    let id: Option<String> = sqlx::query_scalar(
        "SELECT id FROM agent_handover_items \
          WHERE conversation_id = $1 AND state NOT IN ('resolved', 'returned_to_agent') \
          ORDER BY queued_at DESC LIMIT 1",
    )
    .bind(conversation_id)
    .fetch_optional(conn)
    .await?;
    Ok(id)
}

/// 409 `owner_changed`, with the current owner under `owner`.
fn owner_changed(now: &OwnerView) -> ApiError {
    ApiError::Conflict(
        "owner_changed",
        "Someone else changed who owns this conversation.".into(),
    )
    .with("owner", now.json())
}

/// 422 `not_assignable`.
fn not_assignable(msg: &str) -> ApiError {
    ApiError::Unprocessable("not_assignable", msg.to_string())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Verb {
    Take,
    Assign,
    /// Support routing's assignment (`crate::support::routing`): the system assigns, never creates an item, and marks
    /// the item auto-assigned (0204 `auto_assigned_at`) so only the system ever moves it again.
    Auto,
}

/// What a command changed, for the frames after its commit.
pub(crate) struct Changed {
    view: OwnerView,
    /// The changed handover items as `HandoverItem`.
    items: Vec<Value>,
    /// An item the command created (SW-3): `(env, case id)`.
    created: Option<(String, String)>,
}

/// Take: the caller (`actor`) becomes the owner. The caller must hold `support_agent` or `support_manager`, or be the calling
/// super_admin taking it for itself. Only an agent-owned or queued conversation can be taken: 409 `owner_changed`
/// (with the current owner) when the owner is not `expected`, and when another person owns it even if `expected`
/// names them (moving it between people is an Assign). Taking a conversation the caller already owns changes
/// nothing.
pub async fn take(
    st: &AppState,
    j: &JobCtx,
    conv: &str,
    actor: &str,
    expected: Owner,
) -> ApiResult<OwnerView> {
    let mut tx = j.begin(&st.db).await?;
    crate::members::names::actor_seen(&mut tx, &j.t.caller).await?;
    let name = match handover::assignable(&mut tx, actor).await? {
        Some(n) => n,
        None if actor == j.t.caller.sub && j.t.caller.super_admin => {
            store::member_name(&mut tx, actor).await?
        }
        None => {
            return Err(not_assignable(
                "Only an enabled member holding the Support agent or Support manager role can take conversations.",
            ));
        }
    };
    let by = By::from(j);
    let out = own(
        &mut tx,
        &by,
        conv,
        actor,
        actor,
        &name,
        Some(&expected),
        Verb::Take,
        None,
    )
    .await?;
    tx.commit().await?;
    after(st, &by, conv, &out).await;
    Ok(out.view)
}

/// Take by the worker-thread door (SW-5): the staff member the door mapped (`by.sub`, an enabled member holding
/// `support_agent`) answers in the worker thread of item `item_id`. It works like [`take`] with whatever owner the
/// conversation has now: taken from the queue or the agent, nothing changes when it is theirs already, 409
/// `owner_changed` (with the current owner) when a colleague owns it. 409 `closed` when the thread's own item is
/// closed: a reply in an old ticket never acts on a newer case.
pub async fn take_scoped(
    st: &AppState,
    by: &By,
    conv: &str,
    item_id: &str,
) -> ApiResult<OwnerView> {
    let mut tx = store::begin_job(&st.db, by.scope.tenant, by.scope.job_id).await?;
    let name = store::member_name(&mut tx, &by.sub).await?;
    let out = own(
        &mut tx,
        by,
        conv,
        &by.sub,
        &by.sub,
        &name,
        None,
        Verb::Take,
        Some(item_id),
    )
    .await?;
    tx.commit().await?;
    after(st, by, conv, &out).await;
    Ok(out.view)
}

/// Assign (support_manager): `to` becomes the owner. `to` must be an enabled member holding `support_agent` or `support_manager` (422
/// `not_assignable`; a super_admin outside the tenant's members is never offered). 409 `owner_changed` when the
/// owner is not `expected`.
pub async fn assign(
    st: &AppState,
    j: &JobCtx,
    conv: &str,
    actor: &str,
    to: &str,
    expected: Owner,
) -> ApiResult<OwnerView> {
    store::require_clean([to])?;
    let mut tx = j.begin(&st.db).await?;
    crate::members::names::actor_seen(&mut tx, &j.t.caller).await?;
    let name = handover::assignable(&mut tx, to).await?.ok_or_else(|| {
        not_assignable(
            "Assign to an enabled member holding the Support agent or Support manager role.",
        )
    })?;
    // D125: only a member of the job's staff team (or any worker when it has none)
    crate::support::job_staff::require_assignable(&mut tx, j.job_id, to).await?;
    let by = By::from(j);
    let out = own(
        &mut tx,
        &by,
        conv,
        actor,
        to,
        &name,
        Some(&expected),
        Verb::Assign,
        None,
    )
    .await?;
    tx.commit().await?;
    after(st, &by, conv, &out).await;
    Ok(out.view)
}

/// Support routing's Assign (`crate::support::routing`) in the caller's transaction: `to` (see [`handover::assignable`], the one role rule; 422
/// `not_assignable` otherwise) becomes the owner of `conv`'s OPEN item, which routing stamps auto-assigned (0204). The
/// actor is the system (`by.actor`, `by.sub`). `expected` is the owner the routing decision was made on: `Queue` for
/// a new item (a person's Take always wins the race), `User(current)` to move an item routing assigned itself and
/// nobody has touched. 409 `owner_changed` (a normal skip for routing) when the owner is not `expected`, when the
/// conversation has no open item (routing never creates one), and when a reassigned item is a person's choice.
/// Returns what changed; [`after`] publishes it once the caller commits.
pub(crate) async fn assign_scoped_in(
    conn: &mut PgConnection,
    by: &By,
    conv: &str,
    to: &str,
    expected: &Owner,
) -> ApiResult<Changed> {
    store::require_clean([to])?;
    let name = handover::assignable(&mut *conn, to).await?.ok_or_else(|| {
        not_assignable("Routing assigns to an enabled member holding a support role.")
    })?;
    let actor = by.sub.clone();
    own(
        conn,
        by,
        conv,
        &actor,
        to,
        &name,
        Some(expected),
        Verb::Auto,
        None,
    )
    .await
}

/// [`assign_scoped_in`] in its own transaction (scoped to `by.scope`), then the frames: for a caller that needs no
/// transaction of its own.
pub async fn assign_scoped(
    st: &AppState,
    by: &By,
    conv: &str,
    to: &str,
    expected: Owner,
) -> ApiResult<OwnerView> {
    let mut tx = store::begin_job(&st.db, by.scope.tenant, by.scope.job_id).await?;
    let out = assign_scoped_in(&mut tx, by, conv, to, &expected).await?;
    tx.commit().await?;
    after(st, by, conv, &out).await;
    Ok(out.view)
}

/// Take / Assign in the caller's transaction: lock (control lock, owner row, item), compare with `expected` (`None`:
/// whatever owns it now), queue an item when none is open (SW-3, reason `staff_decision`), then make `to` the owner
/// and the item `assigned` (`handover::assign_to`, which logs the event), and audit it. `within`: the item the caller
/// acts in (a worker thread's), 409 `closed` unless it is open in `conv`.
#[allow(clippy::too_many_arguments)]
async fn own(
    conn: &mut PgConnection,
    by: &By,
    conv: &str,
    actor: &str,
    to: &str,
    to_name: &str,
    expected: Option<&Owner>,
    verb: Verb,
    within: Option<&str>,
) -> ApiResult<Changed> {
    store::lock(&mut *conn, &store::control_key(conv)).await?;
    let cur = lock(&mut *conn, conv).await?;
    if let Some(id) = within {
        let it = handover::lock(&mut *conn, id).await?;
        if it.state.closed() || it.conversation_id != conv {
            return Err(closed(id));
        }
    }
    if expected.is_some_and(|e| cur.owner != *e) {
        let item = open_item_id(&mut *conn, conv).await?;
        return Err(owner_changed(&OwnerView::of(&cur, item)));
    }
    let target = Owner::User(to.to_string());
    if cur.owner == target {
        if verb != Verb::Auto {
            // a person acting on an item routing assigned (the assignee taking it, a manager confirming it) makes it
            // theirs: routing never moves it again
            sqlx::query(
                "UPDATE agent_handover_items SET auto_assigned_at = NULL \
                  WHERE conversation_id = $1 AND auto_assigned_at IS NOT NULL \
                    AND state NOT IN ('resolved', 'returned_to_agent')",
            )
            .bind(conv)
            .execute(&mut *conn)
            .await?;
        }
        let (item, stamped) = open_item(&mut *conn, conv).await?;
        return Ok(Changed {
            view: OwnerView::of(&cur, item).with_auto(stamped),
            items: Vec::new(),
            created: None,
        });
    }
    // Take is for an agent-owned or queued conversation; moving it between people is Assign… (support_manager)
    if verb == Verb::Take && matches!(cur.owner, Owner::User(_)) {
        let item = open_item_id(&mut *conn, conv).await?;
        return Err(owner_changed(&OwnerView::of(&cur, item)));
    }
    let actor_name = if verb == Verb::Auto {
        String::new()
    } else {
        store::member_name(&mut *conn, actor).await?
    };
    let mut created = None;
    let item = match handover::open_for_conversation(&mut *conn, conv).await? {
        Some(it) if verb == Verb::Auto => {
            // routing moves an item it assigned itself, or a queued one: a person's choice is never reassigned
            let auto: bool = sqlx::query_scalar(
                "SELECT auto_assigned_at IS NOT NULL FROM agent_handover_items WHERE id = $1 AND conversation_id = $2",
            )
            .bind(&it.id)
            .bind(conv)
            .fetch_one(&mut *conn)
            .await?;
            if matches!(cur.owner, Owner::User(_)) && !auto {
                let item = open_item_id(&mut *conn, conv).await?;
                return Err(owner_changed(&OwnerView::of(&cur, item)));
            }
            it
        }
        Some(it) => it,
        None if verb == Verb::Auto => {
            // routing never creates an item
            return Err(owner_changed(&OwnerView::of(&cur, None)));
        }
        None => {
            // SW-3: an owned conversation always has an open item (SLA, inbox and reporting stay uniform)
            let c = store::conversation_or_404(&mut *conn, conv).await?;
            let design =
                store::design_of(&mut *conn, by.scope.job_id, &by.scope.job_key, &c).await?;
            let case_id = store::case_for(&mut *conn, by.scope.job_id, &c, "", &design).await?;
            let title: Option<String> =
                sqlx::query_scalar("SELECT title FROM agent_case WHERE case_id = $1")
                    .bind(&case_id)
                    .fetch_one(&mut *conn)
                    .await?;
            let trigger = match verb {
                Verb::Take => format!("taken by {to_name}"),
                Verb::Assign => format!("assigned to {to_name}"),
                Verb::Auto => format!("auto-assigned to {to_name}"),
            };
            let id = match handover::create(
                &mut *conn,
                handover::NewItem {
                    job_id: by.scope.job_id,
                    case_id: &case_id,
                    conv: &c,
                    reason: "staff_decision",
                    trigger: &trigger,
                    subject: title.as_deref().unwrap_or(""),
                    actor,
                    design: &design,
                },
            )
            .await
            {
                Ok(id) => id,
                // defence in depth: an item another path queued for this case (the one-open-item index, 23505 →
                // `conflict`; or the pre-check's `already_handed_over`) means the owner moved under us
                Err(e) if matches!(e.code(), "conflict" | "already_handed_over") => {
                    return Err(owner_changed(&OwnerView::of(&cur, None)));
                }
                Err(e) => return Err(e),
            };
            // the same `operation_call` row, case events and `op_calls` as a manual hand-over of the case
            super::routes::manual_handover_call(
                &mut *conn,
                by.scope.job_id,
                &by.sub,
                &case_id,
                &c.env,
                &design,
                &actor_name,
            )
            .await?;
            created = Some((c.env.clone(), case_id));
            handover::lock(&mut *conn, &id).await?
        }
    };
    let note = match verb {
        Verb::Take => format!("{to_name} took it"),
        Verb::Assign => format!("assigned to {to_name}"),
        Verb::Auto => format!("Auto-assigned to {to_name}"),
    };
    handover::assign_to(&mut *conn, by.scope.job_id, &item, to, actor, &note).await?;
    // how it was assigned: routing stamps the item, every person's Take / Assign clears the stamp
    sqlx::query(
        "UPDATE agent_handover_items SET auto_assigned_at = CASE WHEN $2 THEN now() ELSE NULL END WHERE id = $1",
    )
    .bind(&item.id)
    .bind(verb == Verb::Auto)
    .execute(&mut *conn)
    .await?;
    let line = match verb {
        Verb::Take => format!("{actor_name} took this conversation"),
        Verb::Assign => format!("{actor_name} assigned this conversation to {to_name}"),
        Verb::Auto => format!("Auto-assigned to {to_name}"),
    };
    system_row(
        &mut *conn,
        by.scope.job_id,
        conv,
        OWNER_AUTHOR,
        &line,
        Some(&item.case_id),
    )
    .await?;
    let action = match verb {
        Verb::Take => Event::new("conversation.take", "conversation", conv.to_string()),
        Verb::Assign => Event::new("conversation.assign", "conversation", conv.to_string()),
        Verb::Auto => Event::new("conversation.auto_assign", "conversation", conv.to_string()),
    };
    audit::record(
        &mut *conn,
        &by.actor,
        action
            .job(&by.scope.job_key)
            .detail("item_id", item.id.clone())
            .detail("assignee", to.to_string()),
    )
    .await?;
    let now = current(&mut *conn, conv).await?;
    let iv = handover::view(&mut *conn, &item.id).await?;
    Ok(Changed {
        view: OwnerView::of(&now, Some(item.id.clone())).with_auto(verb == Verb::Auto),
        items: vec![iv],
        created,
    })
}

/// Hand back: the owner (`actor`, else 403 `not_owner`) gives the conversation back to the agent with a note (422
/// `note_required`). Every open item goes to `returned_to_agent` with the note on its event and its case reopens;
/// the owner becomes the agent; the note is a `system` message by [`HAND_BACK_AUTHOR`]. A staff owner without an
/// item (a pre-SW console takeover) is handed back the same way.
pub async fn hand_back(
    st: &AppState,
    j: &JobCtx,
    conv: &str,
    actor: &str,
    note: &str,
) -> ApiResult<OwnerView> {
    store::require_clean([note])?;
    let note: String = note.trim().chars().take(2000).collect();
    if note.is_empty() {
        return Err(ApiError::Unprocessable(
            "note_required",
            "Tell the agent what to do next.".into(),
        ));
    }
    let mut tx = j.begin(&st.db).await?;
    crate::members::names::actor_seen(&mut tx, &j.t.caller).await?;
    store::lock(&mut tx, &store::control_key(conv)).await?;
    let cur = lock(&mut tx, conv).await?;
    if cur.owner != Owner::User(actor.to_string()) {
        return Err(ApiError::Forbidden(
            "not_owner",
            "Only the owner can hand this conversation back.".into(),
        ));
    }
    let name = store::member_name(&mut tx, actor).await?;
    let mut closed = Vec::new();
    let mut case_id = None;
    while let Some(item) = handover::open_for_conversation(&mut tx, conv).await? {
        handover::move_to(
            &mut tx,
            j.job_id,
            &item,
            HoState::ReturnedToAgent,
            actor,
            &note,
        )
        .await?;
        store::set_case_status(&mut tx, &item.case_id, "open", None).await?;
        if case_id.is_none() {
            case_id = Some(item.case_id.clone());
        }
        closed.push(item.id);
    }
    // `move_to` already made the agent the owner when it closed the last item; without an item nothing did
    set(&mut tx, conv, &Owner::Agent).await?;
    let case_id = match case_id {
        Some(c) => Some(c),
        None => store::open_case(&mut tx, conv).await?.map(|(c, _)| c),
    };
    let text = format!("Handed back by {name}: {note}");
    system_row(
        &mut tx,
        j.job_id,
        conv,
        HAND_BACK_AUTHOR,
        &text,
        case_id.as_deref(),
    )
    .await?;
    audit::record(
        &mut tx,
        &j.t.actor(),
        Event::new("conversation.hand_back", "conversation", conv.to_string())
            .job(&j.job_key)
            .detail("items", json!(closed)),
    )
    .await?;
    let now = current(&mut tx, conv).await?;
    let mut items = Vec::with_capacity(closed.len());
    for id in &closed {
        items.push(handover::view(&mut tx, id).await?);
    }
    tx.commit().await?;
    let out = Changed {
        view: OwnerView::of(&now, None),
        items,
        created: None,
    };
    after(st, &By::from(j), conv, &out).await;
    Ok(out.view)
}

/// The `system` message author of a Resolve ("Resolved by …", "Closed as … by …").
pub const RESOLUTION_AUTHOR: &str = "resolution";

/// A Resolve's outcomes (0190 `agent_handover_items.resolution`).
pub const OUTCOMES: [&str; 3] = ["resolved", "not_resolvable", "duplicate"];

/// The verdict a Resolve may carry (the feedback route's body).
#[derive(Clone, Debug, Deserialize)]
pub struct Feedback {
    pub verdict: Verdict,
    #[serde(default)]
    pub note: Option<String>,
}

/// Resolve's body: the outcome ([`OUTCOMES`]), an optional closing message to the customer (sent as the owner's
/// reply, with the sender's `client_msg_id` when given) and an optional verdict.
#[derive(Clone, Debug, Deserialize)]
pub struct Resolution {
    pub outcome: String,
    #[serde(default)]
    pub closing_message: Option<String>,
    #[serde(default)]
    pub client_msg_id: Option<uuid::Uuid>,
    #[serde(default)]
    pub feedback: Option<Feedback>,
    /// Whether this Resolve asks the customer for a rating. `None` is the default ([`csat::staff_default`]): off
    /// after a complaint, on otherwise (also when the customer asked for a person).
    #[serde(default)]
    pub ask_rating: Option<bool>,
}

/// 409 `closed`: the item is resolved or returned to the agent already.
fn closed(item_id: &str) -> ApiError {
    ApiError::Conflict("closed", format!("{item_id} is closed."))
}

/// Can item `item_id` (state `state`) of `conversation_id` be resolved: 409 `closed` when it is closed already, 409
/// `invalid_transition` unless it is `assigned` or `waiting_customer` (a queued item is taken first), 409
/// `not_current_item` unless it is the conversation's open item.
async fn resolvable(
    conn: &mut PgConnection,
    item_id: &str,
    conversation_id: &str,
    state: HoState,
) -> ApiResult<()> {
    if state.closed() {
        return Err(closed(item_id));
    }
    if !matches!(state, HoState::Assigned | HoState::WaitingCustomer) {
        return Err(ApiError::Conflict(
            "invalid_transition",
            format!("{item_id} cannot go from {} to resolved.", state.as_str()),
        ));
    }
    if open_item_id(conn, conversation_id).await?.as_deref() != Some(item_id) {
        return Err(ApiError::Conflict(
            "not_current_item",
            format!("{item_id} is not this conversation's open item."),
        ));
    }
    Ok(())
}

/// 403 `not_owner` for a Resolve.
fn not_owner_resolve() -> ApiError {
    ApiError::Forbidden(
        "not_owner",
        "Only the owner can resolve this conversation.".into(),
    )
}

/// Resolve (SW T4): the owner (`actor`, else 403 `not_owner`) closes handover item `item_id` (404 `case_not_found`;
/// 409 `closed`, `invalid_transition`, `not_current_item`: [`resolvable`]) with an outcome (422 `invalid_outcome`). In
/// order:
///
/// 1. A check that changes nothing: the item is `assigned` or `waiting_customer` and the conversation's open item,
///    the caller owns its conversation, and a verdict the Resolve
///    carries is the item's first (409 `feedback_exists`). Nothing is sent when it fails.
/// 2. The closing message, when given, through the one staff-message path (crate::conversations::staff::send, kind
///    reply, at most the channel's limit: 422 `message_too_long`): stored and delivered before the resolution line. A
///    failed delivery does not stop the Resolve: the reply stays `failed` (its `delivery_failed` frame is published,
///    and the Send route delivers it again on a retry with its `client_msg_id`).
/// 3. One transaction in the commands' lock order (control lock, owner row, item), checking again: the item →
///    `resolved` with its `resolution` and `closed_by`, the case → `resolved`, a `system` line by
///    [`RESOLUTION_AUTHOR`], the owner → the agent (SW-4), the verdict (crate::handover::feedback::give), CSAT as a
///    resolve always asked it (SW-6), and the audit row `conversation.resolve` (item id and outcome only).
///
/// After the commit: the `owner` and `handover` frames, then the CSAT frame and request.
pub async fn resolve(
    st: &AppState,
    j: &JobCtx,
    item_id: &str,
    actor: &str,
    r: Resolution,
) -> ApiResult<OwnerView> {
    // a platform caller outside the tenant's users is remembered first, so the lines below read its name
    let mut tx = j.begin(&st.db).await?;
    crate::members::names::actor_seen(&mut tx, &j.t.caller).await?;
    tx.commit().await?;
    resolve_scoped(st, &By::from(j), item_id, actor, r, None).await
}

/// What the worker-thread door adds to a Resolve (crate::conversations::internal `staff-state`): the worker channel
/// (the audit row's `channel`), and the line the customer is told, stored as an `agent` line by `author` after the
/// resolution line and delivered before the CSAT ask.
#[derive(Clone, Copy, Debug)]
pub struct WorkerClose<'a> {
    pub channel: &'a str,
    pub author: &'a str,
    pub customer_text: &'a str,
}

/// [`resolve`] in a scope ([`By`]): the one implementation of the member route and the worker-thread door
/// (`worker`, [`WorkerClose`]). `actor` must own the conversation.
pub async fn resolve_scoped(
    st: &AppState,
    by: &By,
    item_id: &str,
    actor: &str,
    r: Resolution,
    worker: Option<WorkerClose<'_>>,
) -> ApiResult<OwnerView> {
    let fb_note = r
        .feedback
        .as_ref()
        .and_then(|f| f.note.as_deref())
        .unwrap_or_default();
    store::require_clean([
        r.outcome.as_str(),
        r.closing_message.as_deref().unwrap_or_default(),
        fb_note,
    ])?;
    let outcome = r.outcome.trim();
    if !OUTCOMES.contains(&outcome) {
        return Err(ApiError::Unprocessable(
            "invalid_outcome",
            "outcome is resolved, not_resolvable or duplicate.".into(),
        ));
    }
    let closing = r
        .closing_message
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty());
    // 1. the check: nothing is locked across the closing message's delivery
    let (conv, name) = {
        let mut tx = store::begin_job(&st.db, by.scope.tenant, by.scope.job_id).await?;
        let row: Option<(String, String)> =
            sqlx::query_as("SELECT conversation_id, state FROM agent_handover_items WHERE id = $1")
                .bind(item_id)
                .fetch_optional(&mut *tx)
                .await?;
        let (conv_id, state) = row.ok_or_else(handover::not_found)?;
        let state = HoState::parse(&state)
            .ok_or_else(|| ApiError::Internal(anyhow::anyhow!("unknown handover state")))?;
        resolvable(&mut tx, item_id, &conv_id, state).await?;
        if current(&mut tx, &conv_id).await?.owner != Owner::User(actor.to_string()) {
            return Err(not_owner_resolve());
        }
        if r.feedback.is_some() && feedback::of_item(&mut tx, item_id).await?.is_some() {
            return Err(feedback::feedback_exists(item_id));
        }
        let conv = store::conversation_or_404(&mut tx, &conv_id).await?;
        let name = store::member_name(&mut tx, actor).await?;
        tx.commit().await?;
        (conv, name)
    };
    // 2. the closing message, through the Send path
    if let Some(text) = closing {
        let text = text.to_string();
        let text = staff::check_text(&text, staff::channel_limit(&conv.channel))?;
        let sent = staff::send(
            st,
            Reply {
                scope: by.scope.clone(),
                conversation_id: conv.id.clone(),
                kind: MsgKind::Reply,
                actor: by.actor.clone(),
                sub: actor.to_string(),
                name: name.clone(),
                text,
                external_id: None,
                client_msg_id: r.client_msg_id,
                attachments: Vec::new(),
            },
        )
        .await?;
        if let Some(code) = sent.failed {
            tracing::info!(
                conversation = %conv.id,
                item = %item_id,
                code,
                "resolve_closing_message_not_delivered"
            );
        }
    }
    // 3. the resolve, in the commands' lock order
    let mut tx = store::begin_job(&st.db, by.scope.tenant, by.scope.job_id).await?;
    store::lock(&mut tx, &store::control_key(&conv.id)).await?;
    let cur = lock(&mut tx, &conv.id).await?;
    let item = handover::lock(&mut tx, item_id).await?;
    resolvable(&mut tx, item_id, &conv.id, item.state).await?;
    if cur.owner != Owner::User(actor.to_string()) {
        return Err(not_owner_resolve());
    }
    handover::move_to(
        &mut tx,
        by.scope.job_id,
        &item,
        HoState::Resolved,
        actor,
        outcome,
    )
    .await?;
    sqlx::query("UPDATE agent_handover_items SET resolution = $2 WHERE id = $1")
        .bind(&item.id)
        .bind(outcome)
        .execute(&mut *tx)
        .await?;
    store::set_case_status(&mut tx, &item.case_id, "resolved", None).await?;
    let line = match outcome {
        "resolved" => format!("Resolved by {name}."),
        "not_resolvable" => format!("Closed as not resolvable by {name}."),
        _ => format!("Closed as a duplicate by {name}."),
    };
    system_row(
        &mut tx,
        by.scope.job_id,
        &conv.id,
        RESOLUTION_AUTHOR,
        &line,
        Some(&item.case_id),
    )
    .await?;
    // the worker-thread door tells the customer (a customer-facing line, before the CSAT prompt)
    if let Some(w) = &worker {
        store::append(
            &mut tx,
            store::NewMessage {
                job_id: by.scope.job_id,
                conversation_id: &conv.id,
                role: "agent",
                author: w.author,
                text: w.customer_text,
                case_id: Some(&item.case_id),
                external_id: None,
            },
        )
        .await?;
    }
    // SW-4: `move_to` handed it back when no other item is open; a Resolve always does
    set(&mut tx, &conv.id, &Owner::Agent).await?;
    if let Some(f) = &r.feedback {
        feedback::give(
            &mut tx,
            &by.actor,
            by.scope.job_id,
            &by.scope.job_key,
            &item.id,
            f.verdict,
            f.note.as_deref().unwrap_or_default(),
            actor,
        )
        .await?;
    }
    // SW-6: the Resolve's toggle (or the default) asks, on a channel a person reads, where the binding allows it;
    // `ask_staff` is once per conversation under the control lock taken above
    let wants = match r.ask_rating {
        Some(v) => v,
        None => csat::staff_default(&mut tx, &conv.id).await?,
    };
    let csat_asked = wants
        && csat::channel_asks(&conv.channel)
        && csat::enabled(&mut tx, &conv.id).await?
        && csat::ask_staff(&mut tx, &item.case_id, actor).await?;
    if csat_asked {
        // the prompt is a customer-facing line: a lone 1–5 right after it is the rating
        store::append(
            &mut tx,
            store::NewMessage {
                job_id: by.scope.job_id,
                conversation_id: &conv.id,
                role: "agent",
                author: csat::PROMPT_AUTHOR,
                text: csat::PROMPT,
                case_id: Some(&item.case_id),
                external_id: None,
            },
        )
        .await?;
    }
    let mut event = Event::new("conversation.resolve", "conversation", conv.id.clone())
        .job(&by.scope.job_key)
        .detail("item_id", item.id.clone())
        .detail("outcome", outcome.to_string());
    if let Some(w) = &worker {
        event = event.detail("channel", w.channel.to_string());
    }
    audit::record(&mut tx, &by.actor, event).await?;
    let now = current(&mut tx, &conv.id).await?;
    let iv = handover::view(&mut tx, &item.id).await?;
    tx.commit().await?;
    let out = Changed {
        view: OwnerView::of(&now, None),
        items: vec![iv],
        created: None,
    };
    after(st, by, &conv.id, &out).await;
    let s = &by.scope;
    // B2: the conversation is closed: a task it started is no longer waited on
    crate::runtime::task_relay::cancel_open(st, s, &conv.id).await;
    if let Some(w) = &worker {
        let text = w.customer_text.to_string();
        staff::notify_customer(st, s, &conv.id, DeliverKind::Text, &text).await;
    }
    if csat_asked {
        staff::publish(
            st,
            s,
            &conv.id,
            "csat",
            json!({"conversation_id": conv.id, "case_id": item.case_id}),
        )
        .await;
        staff::notify_customer(st, s, &conv.id, DeliverKind::CsatRequest, csat::PROMPT).await;
    }
    Ok(out.view)
}

/// After a command commits: the `owner` frame, a `handover` frame per changed item, and for an item it created the
/// case's `handed_over` phase (as a manual handover does). No worker channel is pinged for that item: the command
/// created it already assigned, so nobody needs to pick it up.
pub(crate) async fn after(st: &AppState, by: &By, conv: &str, out: &Changed) {
    let s = &by.scope;
    staff::publish(st, s, conv, "owner", out.view.frame(conv)).await;
    for item in &out.items {
        staff::publish(st, s, conv, "handover", item.clone()).await;
    }
    if let Some((env, case_id)) = &out.created {
        super::routes::mark_handed_over(st, s, env, case_id).await;
    }
}

/// After the commit of a path outside the commands that queued item `item_id` (the inbound "talk to a person", the
/// runtime's gates, failed and handed-over cases, a manual hand-over): the queue owns its conversation now, so the
/// conversation's `owner` frame is published as it is now, like the commands' ([`OwnerView`]). Best effort: a
/// failure is logged (ids and code only). `handover::notify_workers`, which every such path calls after its commit,
/// calls it.
pub async fn publish_queued(st: &AppState, tenant: uuid::Uuid, job_id: uuid::Uuid, item_id: &str) {
    match owner_frame(st, tenant, job_id, item_id).await {
        Ok(Some((s, conv, frame))) => staff::publish(st, &s, &conv, "owner", frame).await,
        Ok(None) => {}
        Err(e) => tracing::warn!(item = %item_id, code = e.code(), "owner_frame_not_published"),
    }
}

/// The scope, conversation and `owner` frame of item `item_id`'s conversation now; `None` for an unknown item.
async fn owner_frame(
    st: &AppState,
    tenant: uuid::Uuid,
    job_id: uuid::Uuid,
    item_id: &str,
) -> ApiResult<Option<(Scope, String, Value)>> {
    let mut tx = store::begin_job(&st.db, tenant, job_id).await?;
    let conv: Option<String> =
        sqlx::query_scalar("SELECT conversation_id FROM agent_handover_items WHERE id = $1")
            .bind(item_id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some(conv) = conv else {
        tx.commit().await?;
        return Ok(None);
    };
    let job_key = store::job_key(&mut tx, job_id).await?;
    let mut s = read(&mut tx, &conv).await?;
    let (item, stamped) = open_item(&mut tx, &conv).await?;
    tx.commit().await?;
    // the runtime role reads no names: a staff owner's name where this process may read it, else none
    if let Owner::User(sub) = &s.owner {
        s.name = staff_name(st, tenant, job_id, sub).await;
    }
    let frame = OwnerView::of(&s, item).with_auto(stamped).frame(&conv);
    Ok(Some((
        Scope {
            tenant,
            job_id,
            job_key,
        },
        conv,
        frame,
    )))
}

/// [`store::member_name`] in its own transaction; `None` when this process may not read it.
async fn staff_name(
    st: &AppState,
    tenant: uuid::Uuid,
    job_id: uuid::Uuid,
    sub: &str,
) -> Option<String> {
    let mut tx = store::begin_job(&st.db, tenant, job_id).await.ok()?;
    let name = store::member_name(&mut tx, sub).await.ok()?;
    tx.commit().await.ok()?;
    Some(name)
}

async fn named(conn: &mut PgConnection, mut s: OwnerState) -> ApiResult<OwnerState> {
    if let Owner::User(sub) = &s.owner {
        s.name = Some(store::member_name(conn, sub).await?);
    }
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_round_trips() {
        for o in [Owner::Agent, Owner::Queue, Owner::User("a:b".into())] {
            assert_eq!(Owner::parse(&o.wire()), Some(o));
        }
        assert_eq!(Owner::parse("user:"), None);
        assert_eq!(Owner::parse("staff"), None);
        assert!(!silenced(&Owner::Agent));
        assert!(silenced(&Owner::Queue));
        assert!(silenced(&Owner::User("u".into())));
    }
}
