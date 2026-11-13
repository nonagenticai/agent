//! The job-scoped admin routes of conversations, handover and the console (B12 §4, B4 route names and paths).
//!
//! Reads are any member; `assign` (Assign…) is `support_manager`; every other write is `support_agent`; `submitCsat`
//! is any member (a rating changes no tenant data). Send ([`send_message`], SW T3) is routed with the staff test chat
//! on `POST …/conversations/{id}/messages` (crate::routes::chat: a body with `kind` is a Send) and checks its role by
//! kind: a reply is `support_agent` and the owner's, a note any of `support_agent`, `support_manager`, `operator`,
//! `job_designer`. The old reply routes (`/handover/{id}/replies`, `/conversations/{id}/staff-messages`) are
//! retired. The ownership verbs are Take / Assign / Hand back on the conversation and Resolve on its handover item
//! (crate::conversations::ownership, SW T2/T4); claim, take over, take next and the free state route
//! (`/handover/{id}/state`) are retired (the customer's answer moves `waiting_customer → assigned` by itself). An
//! unknown handover item is 404 `case_not_found` (the UI branches on it). Shapes are the web types: `HandoverItem` / `HandoverQueue` (journey.ts), `CaseRow` (runtime.ts),
//! `ConsoleConversation` / `ConsoleSnapshot` / `Conversation` (live.ts), `MessageView` (crate::conversations::store),
//! `SupportTranscript` (support.ts, SW T6). Every surface reads the conversation's owner from one place
//! ([`ownership::view`]) and its channel from the conversation row.
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Json;
use axum::routing::{get, post};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::{PgConnection, Row};

use super::handover;
use super::ownership;
use super::staff::{self, MsgKind, Reply};
use super::{csat, iso, store};
use crate::audit::{self, Event};
use crate::auth::Role;
use crate::error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult, json_body};
use crate::interaction::Scope;
use crate::state::AppState;
use crate::tenancy::{JobCtx, JobMember, JobRequires, role};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/jobs/{job}/handover", get(handover_queue))
        .route(
            "/api/jobs/{job}/handover/{id}/resolve",
            post(resolve_handover),
        )
        .route("/api/jobs/{job}/cases/{id}/handover", post(hand_over_case))
        .route(
            "/api/jobs/{job}/console/conversations",
            get(console_conversations),
        )
        .route("/api/jobs/{job}/conversations/{id}", get(chat_conversation))
        .route(
            "/api/jobs/{job}/conversations/{id}/take",
            post(take_conversation),
        )
        .route(
            "/api/jobs/{job}/conversations/{id}/assign",
            post(assign_conversation),
        )
        .route(
            "/api/jobs/{job}/conversations/{id}/hand-back",
            post(hand_back_conversation),
        )
        .route("/api/jobs/{job}/conversations/{id}/csat", post(submit_csat))
        .route(
            "/api/jobs/{job}/conversations/{id}/events",
            get(crate::sse_hub::conversation_events),
        )
        .route("/api/jobs/{job}/events", get(crate::sse_hub::job_events))
        .merge(super::bindings::routes())
        .merge(super::attachments_routes::routes())
        .merge(super::attachment_settings::routes())
}

fn scope(j: &JobCtx) -> Scope {
    Scope::from(j)
}

/// The caller's display name, for a staff write that records the caller's sub: a super_admin outside the tenant's
/// users is remembered first (`<preferred_username> (platform)`, 0115) so every later name lookup of the sub reads it.
async fn me(conn: &mut PgConnection, j: &JobCtx) -> ApiResult<String> {
    crate::members::names::actor_seen(conn, &j.t.caller).await?;
    store::member_name(conn, &j.t.caller.sub).await
}

// ---- handover queue ---------------------------------------------------------------------------------------------

#[derive(Deserialize)]
struct QueueQuery {
    state: Option<String>,
    assignee: Option<String>,
}

async fn handover_queue(
    JobMember(j): JobMember,
    State(st): State<AppState>,
    ApiQuery(q): ApiQuery<QueueQuery>,
) -> ApiResult<Json<Value>> {
    let mut tx = j.begin(&st.db).await?;
    // D125: a support_agent off a non-empty team sees an empty queue (as in the inbox), except items they own
    let only_for =
        crate::support::job_staff::restricted(j.t.effective).then(|| j.t.caller.sub.clone());
    let v = handover::queue(
        &mut tx,
        q.state.as_deref(),
        q.assignee.as_deref(),
        only_for.as_deref(),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(v))
}

async fn publish_item(st: &AppState, j: &JobCtx, conversation_id: &str, item: &Value) {
    staff::publish(st, &scope(j), conversation_id, "handover", item.clone()).await;
}

// ---- cases ------------------------------------------------------------------------------------------------------

/// One case as the web's `CaseRow`; 404 `case_not_found`.
pub async fn case_row(conn: &mut PgConnection, j: &JobCtx, case_id: &str) -> ApiResult<Value> {
    let r = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT case_id, tenant_id, conversation_id, channel, env, COALESCE(activity, '') AS activity, \
                COALESCE(request_type, '') AS request_type, COALESCE(title, '') AS title, status, outcome, \
                escalation_reason, {level} AS identity_level, \
                COALESCE(customer_id, '') AS customer_id, COALESCE(customer_name, '') AS customer_name, \
                initiator_sub, release_id::text AS release_id, draft_rev, started_at, updated_at, alert, \
                COALESCE(op_calls, 0)::int4 AS op_calls \
           FROM agent_case WHERE case_id = $1",
        level = super::state::level_sql("agent_case"),
    )))
    .bind(case_id)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or_else(|| ApiError::Missing("case_not_found", "No such case in this job.".into()))?;
    let item = sqlx::query(
        "SELECT assignee, resolve_due, queued_at FROM agent_handover_items_owned \
          WHERE case_id = $1 ORDER BY (state IN ('resolved', 'returned_to_agent')), queued_at DESC LIMIT 1",
    )
    .bind(case_id)
    .fetch_optional(&mut *conn)
    .await?;
    let (assignee, sla) = match &item {
        Some(i) => {
            let a: Option<String> = i.get("assignee");
            let due: Option<chrono::DateTime<chrono::Utc>> = i.get("resolve_due");
            let queued: chrono::DateTime<chrono::Utc> = i.get("queued_at");
            let sla = due.map(
                |d| json!({"hours": ((d - queued).num_minutes() as f64 / 60.0), "due_at": iso(d)}),
            );
            (a, sla)
        }
        None => (None, None),
    };
    let assignee_name = match &assignee {
        Some(a) => Some(store::member_name(conn, a).await?),
        None => None,
    };
    // SW T6: the channel is the conversation's, and the owner the conversation's owner record (when the case has a
    // conversation row), as on every other surface
    let conversation_id: String = r.get("conversation_id");
    let conv = store::conversation(conn, &conversation_id).await?;
    let owner = match &conv {
        Some(c) => Some(ownership::view(conn, &c.id).await?),
        None => None,
    };
    let channel = conv.map_or_else(|| r.get::<String, _>("channel"), |c| c.channel);
    let mut v = json!({
        "case_id": r.get::<String, _>("case_id"),
        "tenant_id": r.get::<uuid::Uuid, _>("tenant_id"),
        "job": j.job_key,
        "conversation_id": conversation_id,
        "channel": channel,
        "env": r.get::<String, _>("env"),
        "activity": r.get::<String, _>("activity"),
        "request_type": r.get::<String, _>("request_type"),
        "title": r.get::<String, _>("title"),
        "status": r.get::<String, _>("status"),
        "outcome": r.get::<Option<String>, _>("outcome"),
        "escalation_reason": r.get::<Option<String>, _>("escalation_reason"),
        "identity_level": r.get::<i32, _>("identity_level"),
        "customer_id": r.get::<String, _>("customer_id"),
        "customer_name": r.get::<String, _>("customer_name"),
        "initiator_sub": r.get::<Option<String>, _>("initiator_sub"),
        "release_id": r.get::<Option<String>, _>("release_id"),
        "draft_rev": r.get::<Option<i64>, _>("draft_rev"),
        "started_at": iso(r.get("started_at")),
        "updated_at": iso(r.get("updated_at")),
        "sla": sla,
        "assignee": assignee,
        "assignee_name": assignee_name,
        "alert": r.get::<Option<String>, _>("alert"),
        "op_calls": r.get::<i32, _>("op_calls"),
    });
    if let Some(o) = owner {
        v["owner"] = json!(o);
    }
    Ok(v)
}

const CLOSED_CASE: [&str; 4] = ["resolved", "not_eligible", "failed", "cancelled_offboard"];

#[derive(Deserialize)]
struct HandOverBody {
    reason: String,
}

async fn hand_over_case(
    JobMember(j): JobMember,
    State(st): State<AppState>,
    ApiPath((_, id)): ApiPath<(String, String)>,
    ApiJson(raw): ApiJson<Value>,
) -> ApiResult<Json<Value>> {
    // the role is checked before the body is read (tests/roles.rs ANY_OF)
    j.t.require_conversation_worker()?;
    // D125: the team rule; the conversation's current owner passes
    crate::support::job_staff::require_staff_or_owner(
        &st,
        &j,
        crate::support::job_staff::Target::Case(&id),
    )
    .await?;
    let b: HandOverBody = json_body(raw)?;
    store::require_clean([b.reason.as_str()])?;
    let reason: String = b.reason.trim().chars().take(500).collect();
    let mut tx = j.begin(&st.db).await?;
    // the conversation's control lock and owner row first, before the case row (ownership::guard): the commands'
    // lock order, so a Take creating an item for this case cannot wait on this case row while this waits on the owner
    let conv_id: Option<String> =
        sqlx::query_scalar("SELECT conversation_id FROM agent_case WHERE case_id = $1")
            .bind(&id)
            .fetch_optional(&mut *tx)
            .await?;
    if let Some(c) = &conv_id {
        ownership::guard(&mut tx, c).await?;
    }
    let row = sqlx::query(
        "SELECT conversation_id, status, title FROM agent_case WHERE case_id = $1 FOR UPDATE",
    )
    .bind(&id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| ApiError::Missing("case_not_found", "No such case in this job.".into()))?;
    let status: String = row.get("status");
    if CLOSED_CASE.contains(&status.as_str()) {
        return Err(ApiError::Conflict(
            "case_closed",
            format!("{id} is closed ({status})."),
        ));
    }
    let conv =
        store::conversation_or_404(&mut tx, &row.get::<String, _>("conversation_id")).await?;
    let name = me(&mut tx, &j).await?;
    let trigger = if reason.is_empty() {
        format!("manual handover by {name}")
    } else {
        format!("manual handover by {name}: {reason}")
    };
    let title: Option<String> = row.get("title");
    let design = store::design_of(&mut tx, j.job_id, &j.job_key, &conv).await?;
    let item_id = handover::create(
        &mut tx,
        handover::NewItem {
            job_id: j.job_id,
            case_id: &id,
            conv: &conv,
            reason: "staff_decision",
            trigger: &trigger,
            subject: title.as_deref().unwrap_or(""),
            actor: &j.t.caller.sub,
            design: &design,
        },
    )
    .await?;
    let call_id = manual_handover_call(
        &mut tx,
        j.job_id,
        &j.t.caller.sub,
        &id,
        &conv.env,
        &design,
        &name,
    )
    .await?;
    audit::record(
        &mut tx,
        &j.t.actor(),
        Event::new("case.handover", "case", id.clone())
            .job(&j.job_key)
            .detail("item_id", item_id.clone())
            .detail("call_id", call_id)
            .detail("reason", "staff_decision"),
    )
    .await?;
    let v = case_row(&mut tx, &j, &id).await?;
    let item = handover::view(&mut tx, &item_id).await?;
    tx.commit().await?;
    publish_item(&st, &j, &conv.id, &item).await;
    handover::notify_workers(&st, j.t.tenant, j.job_id, &item_id).await;
    mark_handed_over(&st, &scope(&j), &conv.env, &id).await;
    Ok(Json(v))
}

/// The operation a manual handover records (B4 T12): routed `fallback` to `agent_cases`, as the runner routes it.
const MANUAL_HANDOVER_OP: &str = "case.create{handover}";

/// B4 T12: the `operation_call` row of a manual handover (`case.create{handover}`, route `fallback`, instance
/// `agent_cases`, finished `ok`, stamped with the case's design), its `operation` and `state` case events and the
/// case's `op_calls`, on the handover's transaction. The detail names the caller, never the reason (K50). Returns
/// the call id.
pub(super) async fn manual_handover_call(
    conn: &mut PgConnection,
    job_id: uuid::Uuid,
    initiator: &str,
    case_id: &str,
    env: &str,
    design: &crate::design::CaseDesign,
    name: &str,
) -> ApiResult<String> {
    let (release_id, draft_rev) = store::stamp(design);
    let call_id = format!("opc_{}", uuid::Uuid::now_v7());
    let detail: String = format!("manual handover by {name}")
        .chars()
        .take(2000)
        .collect();
    sqlx::query(
        "INSERT INTO operation_call (tenant_id, job_id, call_id, case_id, operation, route, instance_id, env, \
                                     state, outcome_code, finished_at, latency_ms, initiator, attempt, detail, \
                                     release_id, draft_rev) \
         VALUES (app_current_tenant(), $1, $2, $3, $4, 'fallback', 'agent_cases', $5, 'ok', 'ok', now(), 0, $6, 1, \
                 $7, $8, $9)",
    )
    .bind(job_id)
    .bind(&call_id)
    .bind(case_id)
    .bind(MANUAL_HANDOVER_OP)
    .bind(env)
    .bind(initiator)
    .bind(&detail)
    .bind(release_id)
    .bind(draft_rev)
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "UPDATE agent_case SET op_calls = op_calls + 1, updated_at = now() WHERE case_id = $1",
    )
    .bind(case_id)
    .execute(&mut *conn)
    .await?;
    let events = [
        (
            "operation",
            json!({"call_id": call_id, "operation": MANUAL_HANDOVER_OP, "route": "fallback", "state": "ok",
                   "code": "ok", "latency_ms": 0, "attempt": 1}),
        ),
        (
            "state",
            json!({"status": "handover", "note": "manual handover", "reason": "staff_decision"}),
        ),
    ];
    for (kind, body) in events {
        sqlx::query(
            "INSERT INTO case_event (tenant_id, job_id, case_id, kind, body, by_sub) \
             VALUES (app_current_tenant(), $1, $2, $3, $4, $5)",
        )
        .bind(job_id)
        .bind(case_id)
        .bind(kind)
        .bind(body)
        .bind(initiator)
        .execute(&mut *conn)
        .await?;
    }
    Ok(call_id)
}

/// After a manual handover commits: `phase=handed_over` on the case hash, so the orchestrator stops the case at its
/// next boundary. Best effort: the orchestrator also reads `agent_case.status` there, and no runtime bus (or Redis
/// down) never fails the handover.
pub(super) async fn mark_handed_over(st: &AppState, s: &Scope, env: &str, case_id: &str) {
    let Some(root) = st.redis.as_ref() else {
        return;
    };
    let Ok(rs) = crate::turn::runtime_scope(s, env) else {
        return;
    };
    let cases = crate::runtime::case::CaseStore::new(root.scoped(&rs));
    if let Err(e) = cases.mark_handed_over(case_id).await {
        tracing::warn!(case = %case_id, error = %e, "manual handover: phase not marked");
    }
}

// ---- console ----------------------------------------------------------------------------------------------------

/// One console page.
pub const CONSOLE_PAGE: i64 = 50;

/// One conversation as the web's `ConsoleConversationRow` (types/conversations.ts); `None` when it does not exist.
async fn console_row(conn: &mut PgConnection, id: &str) -> ApiResult<Option<Value>> {
    let Some(conv) = store::conversation(conn, id).await? else {
        return Ok(None);
    };
    let case = sqlx::query(
        "SELECT case_id, status, updated_at FROM agent_case WHERE conversation_id = $1 \
          ORDER BY started_at DESC, case_id DESC LIMIT 1",
    )
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?;
    let (started, party_id): (chrono::DateTime<chrono::Utc>, Option<uuid::Uuid>) =
        sqlx::query_as("SELECT started_at, party_id FROM conversation WHERE conversation_id = $1")
            .bind(id)
            .fetch_one(&mut *conn)
            .await?;
    let party = crate::party::brief(conn, party_id).await?;
    let csat = handover::csat_of_conversation(conn, id).await?;
    let owner = ownership::view(conn, id).await?;
    let display_name = customer_display_name(conn, &conv).await?;
    let last = store::last_messages(conn, id, 1).await?.pop();
    let mut updated = started;
    if let Some(c) = &case {
        updated = updated.max(c.get("updated_at"));
    }
    if let Some(m) = &last
        && let Some(t) = chrono::DateTime::from_timestamp_millis(m.at_ms)
    {
        updated = updated.max(t);
    }
    Ok(Some(json!({
        "conversation_id": conv.id,
        "channel": conv.channel,
        "customer_name": conv.customer_name.clone().or(conv.customer_id.clone()),
        "display_name": display_name,
        "case_id": case.as_ref().map(|c| c.get::<String, _>("case_id")),
        "status": case.as_ref().map_or_else(|| "open".to_string(), |c| c.get::<String, _>("status")),
        "open_item_id": owner.item_id.clone(),
        "owner": owner,
        "last_message": last.map(|m| json!({"role": m.role, "text": m.text, "at_ms": m.at_ms})),
        "updated_at": iso(updated),
        "party": party,
        "csat": csat,
    })))
}

#[derive(Deserialize)]
struct ConsoleQuery {
    before: Option<String>,
}

/// `consoleConversations`: the job's conversations, most recently active first, paged by `before` (the `next` of
/// the previous page: an activity instant in epoch ms).
async fn console_conversations(
    JobMember(j): JobMember,
    State(st): State<AppState>,
    ApiQuery(q): ApiQuery<ConsoleQuery>,
) -> ApiResult<Json<Value>> {
    let before = crate::page::id_cursor(q.before.as_deref())?;
    let mut tx = j.begin(&st.db).await?;
    let rows = sqlx::query(
        "SELECT conversation_id, last_ms FROM ( \
             SELECT c.conversation_id, \
                    COALESCE((SELECT max(m.at_ms) FROM conversation_message m \
                               WHERE m.conversation_id = c.conversation_id), \
                             (extract(epoch FROM c.started_at) * 1000)::bigint) AS last_ms \
               FROM conversation c) t \
          WHERE $1::bigint IS NULL OR last_ms < $1 \
          ORDER BY last_ms DESC, conversation_id LIMIT $2",
    )
    .bind(before)
    .bind(CONSOLE_PAGE + 1)
    .fetch_all(&mut *tx)
    .await?;
    let more = rows.len() as i64 > CONSOLE_PAGE;
    let mut items = Vec::new();
    let mut next = None;
    for r in rows.iter().take(CONSOLE_PAGE as usize) {
        let id: String = r.get("conversation_id");
        if let Some(v) = console_row(&mut tx, &id).await? {
            items.push(v);
        }
        next = Some(r.get::<i64, _>("last_ms").to_string());
    }
    tx.commit().await?;
    Ok(Json(crate::page::paged(
        items,
        if more { next } else { None },
    )))
}

#[derive(Deserialize)]
struct TakeBody {
    expected_owner: String,
}

/// Take (SW T2): the caller owns the conversation; `OwnerView`.
async fn take_conversation(
    JobMember(j): JobMember,
    State(st): State<AppState>,
    ApiPath((_, id)): ApiPath<(String, String)>,
    ApiJson(raw): ApiJson<Value>,
) -> ApiResult<Json<ownership::OwnerView>> {
    // the role is checked before the body is read (tests/roles.rs ANY_OF)
    j.t.require_conversation_worker()?;
    // D125: a support_agent takes only conversations of jobs whose staff team is empty or holds them
    crate::support::job_staff::require_staff(&st, &j).await?;
    let b: TakeBody = json_body(raw)?;
    let expected = ownership::expected(&b.expected_owner)?;
    let sub = j.t.caller.sub.clone();
    Ok(Json(ownership::take(&st, &j, &id, &sub, expected).await?))
}

#[derive(Deserialize)]
struct AssignBody {
    user_sub: String,
    expected_owner: String,
}

/// Assign… (SW T2, support_manager): `user_sub` owns the conversation; `OwnerView`.
async fn assign_conversation(
    JobRequires(j, _): JobRequires<role::SupportManager>,
    State(st): State<AppState>,
    ApiPath((_, id)): ApiPath<(String, String)>,
    ApiJson(b): ApiJson<AssignBody>,
) -> ApiResult<Json<ownership::OwnerView>> {
    let expected = ownership::expected(&b.expected_owner)?;
    let sub = j.t.caller.sub.clone();
    Ok(Json(
        ownership::assign(&st, &j, &id, &sub, b.user_sub.trim(), expected).await?,
    ))
}

#[derive(Deserialize)]
struct HandBackBody {
    #[serde(default)]
    note: String,
}

/// Hand back (SW T2): the owner gives the conversation back to the agent with a note; `OwnerView`.
async fn hand_back_conversation(
    JobMember(j): JobMember,
    State(st): State<AppState>,
    ApiPath((_, id)): ApiPath<(String, String)>,
    ApiJson(raw): ApiJson<Value>,
) -> ApiResult<Json<ownership::OwnerView>> {
    // the role is checked before the body is read (tests/roles.rs ANY_OF)
    j.t.require_conversation_worker()?;
    // D125: the team rule; the owner passes (removal from a team does not strand their conversation)
    crate::support::job_staff::require_staff_or_owner(
        &st,
        &j,
        crate::support::job_staff::Target::Conversation(&id),
    )
    .await?;
    let b: HandBackBody = json_body(raw)?;
    let sub = j.t.caller.sub.clone();
    Ok(Json(
        ownership::hand_back(&st, &j, &id, &sub, &b.note).await?,
    ))
}

/// Resolve (SW T4): the owner closes handover item `id` with `{outcome, closing_message?, client_msg_id?, feedback?}`;
/// the owner returns to the agent. `OwnerView`.
async fn resolve_handover(
    JobMember(j): JobMember,
    State(st): State<AppState>,
    ApiPath((_, id)): ApiPath<(String, String)>,
    ApiJson(raw): ApiJson<Value>,
) -> ApiResult<Json<ownership::OwnerView>> {
    // the role is checked before the body is read (tests/roles.rs ANY_OF)
    j.t.require_conversation_worker()?;
    // D125: the team rule; the owner passes
    crate::support::job_staff::require_staff_or_owner(
        &st,
        &j,
        crate::support::job_staff::Target::Item(&id),
    )
    .await?;
    let b: ownership::Resolution = json_body(raw)?;
    let sub = j.t.caller.sub.clone();
    Ok(Json(ownership::resolve(&st, &j, &id, &sub, b).await?))
}

/// The staff roles that work a conversation: who may write a note (SW T3) and give a handover verdict (SW T4,
/// crate::routes::handover).
pub const STAFF_ROLES: [Role; 4] = [
    Role::SupportAgent,
    Role::SupportManager,
    Role::Operator,
    Role::JobDesigner,
];

/// Send (SW T3): `{kind: "reply" | "note", text, client_msg_id}` on conversation `id` → 200 `MessageView`, or 202
/// `MessageView` with `delivery: "failed"` (the reply is kept; a `delivery_failed` frame is published). A reply is
/// `support_agent` (403 `forbidden`) and the owner's (403 `not_owner`), at most the channel's limit
/// ([`staff::channel_limit`]: 422 `message_too_long` with `limit`); a note is any of [`STAFF_ROLES`] (else 403
/// `note_not_allowed`), at most
/// [`store::MAX_TEXT`], and never delivered. A second Send with the same `client_msg_id` answers the stored message;
/// a reply whose delivery failed is delivered again (by its owner).
/// 422 `invalid_kind`, `empty_message`; 404 `conversation_not_found`. Called by crate::routes::chat, which owns the
/// path with the staff test chat.
pub(crate) async fn send_message(
    st: &AppState,
    j: &JobCtx,
    id: &str,
    kind: &str,
    text: &str,
    client_msg_id: Option<uuid::Uuid>,
    attachments: Vec<uuid::Uuid>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    store::require_clean([kind])?;
    let kind = MsgKind::parse(kind)
        .ok_or_else(|| ApiError::Unprocessable("invalid_kind", "kind is reply or note.".into()))?;
    match kind {
        MsgKind::Reply => {
            j.t.require_conversation_worker()?;
            // D125: the team rule of Take, but the conversation's current owner passes
            crate::support::job_staff::require_staff_or_owner(
                st,
                j,
                crate::support::job_staff::Target::Conversation(id),
            )
            .await?;
        }
        MsgKind::Note => {
            // D125: a support_agent writes notes on a job's conversations under the same team rule (owner passes)
            crate::support::job_staff::require_staff_or_owner(
                st,
                j,
                crate::support::job_staff::Target::Conversation(id),
            )
            .await?;
            if !STAFF_ROLES.iter().any(|r| j.t.effective.has(*r)) {
                return Err(ApiError::Forbidden(
                    "note_not_allowed",
                    "Notes are for support agents, support managers, operators and job designers."
                        .into(),
                ));
            }
        }
    }
    let (conv, name) = {
        let mut tx = j.begin(&st.db).await?;
        let conv = store::conversation_or_404(&mut tx, id).await?;
        let n = me(&mut tx, j).await?;
        tx.commit().await?;
        (conv, n)
    };
    if !attachments.is_empty() {
        // SW-9: only a reply, only where the channel binding allows staff files, at most 5 (422, nothing is stored)
        if kind != MsgKind::Reply {
            return Err(ApiError::Unprocessable(
                "attachments_on_note",
                "A note carries no attachments.".into(),
            ));
        }
        if attachments.len() > staff::MAX_ATTACHMENTS {
            return Err(ApiError::Unprocessable(
                "too_many_attachments",
                format!("A reply carries at most {} files.", staff::MAX_ATTACHMENTS),
            )
            .with("limit", staff::MAX_ATTACHMENTS));
        }
        // the admin's setting on the binding the customer arrived on (default: on for web chat and Telegram only)
        let mut tx = j.begin(&st.db).await?;
        let switches =
            super::attachment_settings::for_conversation(&mut tx, &conv.id, &conv.channel).await?;
        tx.commit().await?;
        if !switches.staff {
            return Err(super::attachment_settings::staff_off());
        }
    }
    let limit = match kind {
        MsgKind::Reply => staff::channel_limit(&conv.channel),
        MsgKind::Note => store::MAX_TEXT,
    };
    let text = staff::check_reply_text(text, limit, !attachments.is_empty())?;
    let sent = staff::send(
        st,
        Reply {
            scope: scope(j),
            conversation_id: conv.id.clone(),
            kind,
            actor: j.t.actor(),
            sub: j.t.caller.sub.clone(),
            name,
            text,
            external_id: None,
            client_msg_id,
            attachments,
        },
    )
    .await?;
    let status = if sent.view.delivery == Some("failed") {
        StatusCode::ACCEPTED
    } else {
        StatusCode::OK
    };
    Ok((status, Json(json!(sent.view))))
}

// ---- conversation -----------------------------------------------------------------------------------------------

/// The customer's name as staff read it ([`crate::support::names::display_name`]): the party's name, else the channel
/// profile's name, else the channel's fallback handle (never a raw channel id).
async fn customer_display_name(conn: &mut PgConnection, conv: &store::Conv) -> ApiResult<String> {
    let party: Option<Option<String>> = sqlx::query_scalar(
        "SELECT p.display_name FROM conversation v JOIN party p ON p.id = v.party_id \
          WHERE v.conversation_id = $1",
    )
    .bind(&conv.id)
    .fetch_optional(conn)
    .await?;
    Ok(crate::support::names::display_name(
        party.flatten().as_deref(),
        conv.customer_name.as_deref(),
        &conv.channel,
        &conv.id,
    ))
}

/// The most messages one transcript read returns (the newest).
const TRANSCRIPT_LIMIT: i64 = 500;

/// `transcript` (SW T6, the web's `SupportTranscript`), the one projection every surface reads: `{conversation, owner,
/// messages, episodes}`. `conversation` is `SupportConversation` `{conversation_id, job, channel, display_name,
/// verified, case_id}` (the newest case); `owner` is the conversation's [`ownership::OwnerView`]; `messages` are typed
/// `MessageView`s (notes and `system` rows included), oldest first; `episodes` are its handover items
/// ([`handover::episodes`]). 404 `conversation_not_found`.
///
/// The staff test chat (web `chatConversation`, B4 T9 / F-13) reads this route too, so the answer also carries the
/// web `Conversation` fields (`conversation_id`, `tenant_id`, `job`, `customer_id`, `env`, `initiator_sub`,
/// `case_ids`, `csat_request`), and each message the B12 `ChatEvent` fields (`conversation_id`, `role`, `author`,
/// `at_ms`; `refused` on a refused turn's reply, `trace` on a test-env reply), as the `message` frame does.
async fn chat_conversation(
    JobMember(j): JobMember,
    State(st): State<AppState>,
    ApiPath((_, id)): ApiPath<(String, String)>,
) -> ApiResult<Json<Value>> {
    let mut tx = j.begin(&st.db).await?;
    let conv = store::conversation_or_404(&mut tx, &id).await?;
    let owner = ownership::view(&mut tx, &id).await?;
    let display_name = customer_display_name(&mut tx, &conv).await?;
    let case_id = store::latest_case(&mut tx, &id).await?;
    let refused: Vec<uuid::Uuid> = sqlx::query_scalar(
        "SELECT message_id FROM conversation_message WHERE conversation_id = $1 AND refused",
    )
    .bind(&id)
    .fetch_all(&mut *tx)
    .await?;
    // the test-env reply trace ("How the agent answered") is kept on the case's `reply` event; the live `message`
    // frame carries it too, but a transcript read must not lose it (the web keeps the first copy of a message)
    let traces: std::collections::HashMap<uuid::Uuid, Value> = sqlx::query_as::<_, (uuid::Uuid, Value)>(
        "SELECT (e.body->>'message_id')::uuid, e.body->'trace' FROM case_event e \
           JOIN agent_case c ON c.tenant_id = e.tenant_id AND c.job_id = e.job_id AND c.case_id = e.case_id \
          WHERE c.conversation_id = $1 AND e.kind = 'reply' AND e.body ? 'trace' AND e.body ? 'message_id'",
    )
    .bind(&id)
    .fetch_all(&mut *tx)
    .await?
    .into_iter()
    .collect();
    let messages: Vec<Value> = store::last_views(&mut tx, &id, TRANSCRIPT_LIMIT)
        .await?
        .into_iter()
        .map(|(v, author, at_ms)| {
            let mut e = staff::message_frame(&id, &v, &author, at_ms);
            if refused.contains(&v.message_id) {
                e["refused"] = json!(true);
            }
            if let Some(t) = traces.get(&v.message_id) {
                e["trace"] = t.clone();
            }
            e
        })
        .collect();
    let episodes = handover::episodes(&mut tx, &id).await?;
    let case_ids: Vec<String> = sqlx::query_scalar(
        "SELECT case_id FROM agent_case WHERE conversation_id = $1 ORDER BY started_at, case_id",
    )
    .bind(&id)
    .fetch_all(&mut *tx)
    .await?;
    let csat_request = csat::latest(&mut tx, &id).await?;
    // SW-9: what the admin allowed on this conversation's channel; the composer's paperclip follows `staff`
    let attachments =
        super::attachment_settings::for_conversation(&mut tx, &id, &conv.channel).await?;
    tx.commit().await?;
    Ok(Json(json!({
        "conversation": {
            "attachments": attachments,
            "conversation_id": conv.id,
            "job": j.job_key,
            "channel": conv.channel,
            "display_name": display_name,
            "verified": matches!(conv.identity_source.as_deref(), Some("assertion" | "staff_pick" | "staff_test")),
            "case_id": case_id,
        },
        "owner": owner,
        "messages": messages,
        "episodes": episodes,
        // the staff test chat's `Conversation` fields
        "conversation_id": conv.id,
        "tenant_id": j.t.tenant,
        "job": j.job_key,
        "customer_id": conv.customer_id,
        "env": conv.env,
        "initiator_sub": conv.initiator_sub,
        "case_ids": case_ids,
        "csat_request": csat_request,
    })))
}

#[derive(Deserialize)]
struct CsatBody {
    case_id: String,
    rating: i64,
}

async fn submit_csat(
    JobMember(j): JobMember,
    State(st): State<AppState>,
    ApiPath((_, id)): ApiPath<(String, String)>,
    ApiJson(b): ApiJson<CsatBody>,
) -> ApiResult<Json<Value>> {
    let rating = csat::check_rating(b.rating)?;
    let mut tx = j.begin(&st.db).await?;
    store::conversation_or_404(&mut tx, &id).await?;
    // a low score asks no follow-up here: the staff demo console is not the customer's channel
    let _low = csat::record(&mut tx, &id, &b.case_id, rating).await?;
    audit::record(
        &mut tx,
        &j.t.actor(),
        Event::new("case.csat", "case", b.case_id.clone())
            .job(&j.job_key)
            .detail("rating", rating),
    )
    .await?;
    let v = csat::latest(&mut tx, &id).await?;
    tx.commit().await?;
    staff::publish(&st, &scope(&j), &id, "csat", v.clone()).await;
    Ok(Json(v))
}
