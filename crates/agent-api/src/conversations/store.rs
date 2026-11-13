//! The conversation store (B4 0060/0061 shapes, B12 §1/§2 deltas): conversations found by their channel ref
//! (`channel_conversation_refs`), cases (`CS-n`, `agent_next_case_id()`), the message log (`conversation_message`, a
//! provider retry deduped on `external_id`, a staff Send on `client_msg_id`), the history a reply is built from, and
//! console control.
//!
//! A `note` (SW T3, 0191) is staff-only: every read that leaves the platform (the model's history, the customer's
//! history, the last customer-facing line, a worker channel's excerpt) leaves it out.
//!
//! Every function takes a transaction opened by [`begin_job`] (tenant and job GUCs set: RLS confines it to the job).
use sqlx::{PgConnection, PgPool, Postgres, Row, Transaction};
use uuid::Uuid;

use crate::design::{self, CaseDesign, JobRef};
use crate::error::{ApiError, ApiResult};
use crate::interaction::ConversationRef;

/// Case statuses that are still open (the other four close a case).
pub const OPEN_CASE: [&str; 4] = [
    "open",
    "identity_required",
    "confirmation_required",
    "handover",
];

/// Turns of history a reply is built from.
pub const HISTORY_TURNS: i64 = 20;

/// The longest message stored (characters; the interaction API's own limit is larger).
pub const MAX_TEXT: usize = 8_000;

/// A transaction with both GUCs set (`app.current_tenant`, `app.current_job`): the internal door's scope, which has
/// no person and so no [`crate::tenancy::JobCtx`].
pub async fn begin_job(
    db: &PgPool,
    tenant: Uuid,
    job_id: Uuid,
) -> ApiResult<Transaction<'static, Postgres>> {
    let mut tx = crate::tenancy::begin_scoped(db, tenant).await?;
    sqlx::query("SELECT set_config('app.current_job', $1, true)")
        .bind(job_id.to_string())
        .execute(&mut *tx)
        .await?;
    Ok(tx)
}

/// Serialise writers of one key for the rest of the transaction (one inbound conversation at a time).
pub async fn lock(conn: &mut PgConnection, key: &str) -> ApiResult<()> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(key)
        .execute(conn)
        .await?;
    Ok(())
}

/// One conversation row.
#[derive(Clone, Debug)]
pub struct Conv {
    pub id: String,
    pub channel: String,
    pub env: String,
    /// The identified customer's `subject` state when there is one, else the conversation's wire party id.
    pub customer_id: Option<String>,
    /// State `name`.
    pub customer_name: Option<String>,
    pub initiator_sub: Option<String>,
    /// State `identity_source`. How `customer_id` became known: `staff_pick`, `staff_test` (a staff-typed test customer) or `assertion` (0183). NULL = not identified; this column, never
    /// a channel's claim (`WireParty.verified`), is what makes a conversation's customer known.
    pub identity_source: Option<String>,
    /// The KYC'd email of the known customer (state `email`).
    pub customer_email: Option<String>,
    /// What a tenant IAM procedure established (state `identity_strength`): `identified` or `verified`; `None` for
    /// every identity the platform itself established, or none at all.
    pub identity_strength: Option<String>,
}

/// The identity level of a conversation whose customer is known before it starts (owner ruling 2026-10-26).
pub const KNOWN_CUSTOMER_IDENTITY: u8 = 2;

impl Conv {
    /// The verified customer: `customer_id` set AND `identity_source` one of `assertion` / `staff_pick` /
    /// `staff_test`, or a tenant IAM procedure of strength `verified`. A wire-claimed verified flag is never
    /// consulted.
    pub fn known_customer(&self) -> Option<&str> {
        let id = self.customer_id.as_deref().map(str::trim)?;
        let sourced = matches!(
            self.identity_source.as_deref(),
            Some("assertion" | "staff_pick" | "staff_test")
        ) || (self.identity_source.as_deref() == Some("iam")
            && self.identity_strength.as_deref() == Some("verified"));
        (sourced && !id.is_empty()).then_some(id)
    }

    /// The person this conversation is known to be at level 1 or above: a verified customer, or a tenant IAM
    /// procedure of strength `identified`. Never a channel's claim.
    pub fn identified_subject(&self) -> Option<&str> {
        let id = self.customer_id.as_deref().map(str::trim)?;
        let sourced = self.known_customer().is_some()
            || (self.identity_source.as_deref() == Some("iam")
                && matches!(
                    self.identity_strength.as_deref(),
                    Some("identified" | "verified")
                ));
        (sourced && !id.is_empty()).then_some(id)
    }

    /// The level a case of this conversation is born at: [`KNOWN_CUSTOMER_IDENTITY`] for a verified customer (or a
    /// verified IAM procedure), 1 for an identified IAM procedure, else 0.
    pub fn identity_level(&self) -> u8 {
        if self.known_customer().is_some() {
            KNOWN_CUSTOMER_IDENTITY
        } else if self.identified_subject().is_some() {
            1
        } else {
            0
        }
    }

    /// `(verified_party, verified_email)` a case is born with: the known customer and the conversation's KYC email.
    pub fn verified(&self) -> (Option<String>, Option<String>) {
        match self.known_customer() {
            Some(c) => (
                Some(c.to_owned()),
                self.customer_email
                    .as_deref()
                    .map(str::trim)
                    .filter(|e| !e.is_empty())
                    .map(str::to_owned),
            ),
            None => (None, None),
        }
    }
}

/// The columns of a [`Conv`] for the row aliased `c`: the identity fields are read from conversation state (A1), not
/// from `conversation`. `customer_id` is the identified customer's `subject` when state has one, else the
/// conversation's wire party id (the column: `api:{key}`, `int:{id}`, a Telegram chat id, a web chat session).
pub fn conv_columns(c: &str) -> String {
    use super::state as st;
    format!(
        "{c}.conversation_id, {c}.channel, {c}.env, COALESCE({subject}, {c}.customer_id) AS customer_id, \
         {name} AS customer_name, {c}.initiator_sub, {source} AS identity_source, {email} AS customer_email, \
         {strength} AS identity_strength",
        subject = st::sub(c, st::SUBJECT),
        name = st::sub(c, st::NAME),
        source = st::sub(c, st::IDENTITY_SOURCE),
        email = st::sub(c, st::EMAIL),
        strength = st::sub(c, st::IDENTITY_STRENGTH),
    )
}

/// A [`Conv`] from a row selected with [`conv_columns`].
pub fn conv_from_row(r: &sqlx::postgres::PgRow) -> Conv {
    Conv {
        id: r.get("conversation_id"),
        channel: r.get("channel"),
        env: r.get("env"),
        customer_id: r.get("customer_id"),
        customer_name: r.get("customer_name"),
        initiator_sub: r.get("initiator_sub"),
        identity_source: r.get("identity_source"),
        customer_email: r.get("customer_email"),
        identity_strength: r.get("identity_strength"),
    }
}

pub async fn conversation(conn: &mut PgConnection, id: &str) -> ApiResult<Option<Conv>> {
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {} FROM conversation c WHERE c.conversation_id = $1",
        conv_columns("c")
    )))
    .bind(id)
    .fetch_optional(conn)
    .await?;
    Ok(row.as_ref().map(conv_from_row))
}

/// 404 `conversation_not_found` unless the conversation exists in the job.
pub async fn conversation_or_404(conn: &mut PgConnection, id: &str) -> ApiResult<Conv> {
    conversation(conn, id).await?.ok_or_else(|| {
        ApiError::Missing(
            "conversation_not_found",
            "No such conversation in this job.".into(),
        )
    })
}

fn route_json(r: &ConversationRef) -> serde_json::Value {
    serde_json::to_value(&r.route).unwrap_or_else(|_| serde_json::json!({}))
}

/// The customer conversation behind `cref` on `channel` in `env` (D130: the env of the binding it arrived on),
/// created (with its ref) on first contact; the ref's route is refreshed on every message. A ref whose conversation
/// is in the other env (the binding was published or unpublished since) is moved to a NEW conversation: the test
/// and the live conversation of one customer are never the same thread.
pub async fn upsert_customer_conversation(
    conn: &mut PgConnection,
    job_id: Uuid,
    channel: &str,
    cref: &ConversationRef,
    customer_id: &str,
    customer_name: Option<&str>,
    env: &str,
) -> ApiResult<Conv> {
    let found: Option<(String, Option<String>)> = sqlx::query_as(
        "SELECT r.conversation_id, c.env FROM channel_conversation_refs r \
           LEFT JOIN conversation c ON c.conversation_id = r.conversation_id AND c.job_id = r.job_id \
          WHERE r.job_id = $3 AND r.channel = $1 AND r.side = 'customer' AND r.external_id = $2",
    )
    .bind(channel)
    .bind(&cref.id)
    .bind(job_id)
    .fetch_optional(&mut *conn)
    .await?;
    let id = match found {
        Some((id, conv_env)) if conv_env.as_deref().is_none_or(|e| e == env) => {
            sqlx::query(
                "UPDATE channel_conversation_refs SET route = $3, updated_at = now() \
                  WHERE job_id = $4 AND channel = $1 AND side = 'customer' AND external_id = $2",
            )
            .bind(channel)
            .bind(&cref.id)
            .bind(route_json(cref))
            .bind(job_id)
            .execute(&mut *conn)
            .await?;
            id
        }
        other => {
            let id = format!("conv-{}", Uuid::now_v7());
            sqlx::query(
                "INSERT INTO conversation \
                    (tenant_id, job_id, conversation_id, env, channel, customer_id, started_at) \
                 VALUES (app_current_tenant(), $1, $2, $5, $3, $4, now())",
            )
            .bind(job_id)
            .bind(&id)
            .bind(channel)
            .bind(customer_id)
            .bind(env)
            .execute(&mut *conn)
            .await?;
            // SW-1: every conversation has an owner row, the agent's from the start
            super::ownership::set(&mut *conn, &id, &super::ownership::Owner::Agent).await?;
            // A1 dual write: the wire party id of a channel that does not identify is `channel_subject` state (an
            // API key caller writes its own `caller` state and `identity_source = api_key`)
            if channel != "api" {
                super::state::put_channel_subject(
                    &mut *conn,
                    &id,
                    customer_id,
                    &format!("channel:{channel}"),
                )
                .await?;
            }
            // the channel's display name for the customer (a Telegram username …): state `name`, display only; it
            // never makes the customer known (that needs `subject` + a sourcing `identity_source`)
            if let Some(n) = customer_name.map(str::trim).filter(|n| !n.is_empty()) {
                super::state::put_value(
                    &mut *conn,
                    &id,
                    super::state::NAME,
                    n,
                    &format!("channel:{channel}"),
                    None,
                )
                .await?;
            }
            if other.is_some() {
                sqlx::query(
                    "UPDATE channel_conversation_refs SET conversation_id = $3, route = $4, updated_at = now() \
                      WHERE job_id = $5 AND channel = $1 AND side = 'customer' AND external_id = $2",
                )
                .bind(channel)
                .bind(&cref.id)
                .bind(&id)
                .bind(route_json(cref))
                .bind(job_id)
                .execute(&mut *conn)
                .await?;
            } else {
                sqlx::query(
                    "INSERT INTO channel_conversation_refs \
                        (tenant_id, job_id, conversation_id, side, item_id, channel, external_id, route, updated_at) \
                     VALUES (app_current_tenant(), $1, $2, 'customer', NULL, $3, $4, $5, now())",
                )
                .bind(job_id)
                .bind(&id)
                .bind(channel)
                .bind(&cref.id)
                .bind(route_json(cref))
                .execute(&mut *conn)
                .await?;
            }
            id
        }
    };
    conversation_or_404(conn, &id).await
}

/// The customer-side ref of a conversation (where a staff reply is delivered), with the binding it arrived on.
pub async fn customer_ref(
    conn: &mut PgConnection,
    conversation_id: &str,
) -> ApiResult<Option<(ConversationRef, Option<Uuid>)>> {
    let row = sqlx::query(
        "SELECT channel, external_id, route FROM channel_conversation_refs \
          WHERE conversation_id = $1 AND side = 'customer' ORDER BY updated_at DESC LIMIT 1",
    )
    .bind(conversation_id)
    .fetch_optional(conn)
    .await?;
    Ok(row.map(|r| {
        let route: serde_json::Value = r.get("route");
        let route: std::collections::BTreeMap<String, String> =
            serde_json::from_value(route).unwrap_or_default();
        let binding = route
            .get("binding_id")
            .and_then(|b| Uuid::parse_str(b).ok());
        (
            ConversationRef {
                channel: r.get("channel"),
                id: r.get("external_id"),
                route,
            },
            binding,
        )
    }))
}

/// Store the worker thread `thread` of item `item_id` (a worker channel's answer to `notify-workers`).
pub async fn save_worker_ref(
    conn: &mut PgConnection,
    job_id: Uuid,
    conversation_id: &str,
    item_id: &str,
    channel: &str,
    thread: &ConversationRef,
) -> ApiResult<()> {
    sqlx::query(
        "INSERT INTO channel_conversation_refs \
            (tenant_id, job_id, conversation_id, side, item_id, channel, external_id, route, updated_at) \
         VALUES (app_current_tenant(), $1, $2, 'worker', $3, $4, $5, $6, now()) \
         ON CONFLICT (tenant_id, job_id, channel, side, external_id) \
         DO UPDATE SET item_id = EXCLUDED.item_id, conversation_id = EXCLUDED.conversation_id, \
                       route = EXCLUDED.route, updated_at = now()",
    )
    .bind(job_id)
    .bind(conversation_id)
    .bind(item_id)
    .bind(channel)
    .bind(&thread.id)
    .bind(route_json(thread))
    .execute(conn)
    .await?;
    Ok(())
}

/// The item and conversation a worker thread belongs to.
pub async fn worker_thread(
    conn: &mut PgConnection,
    channel: &str,
    external_id: &str,
) -> ApiResult<Option<(String, Option<String>)>> {
    let row = sqlx::query(
        "SELECT conversation_id, item_id FROM channel_conversation_refs \
          WHERE channel = $1 AND side = 'worker' AND external_id = $2",
    )
    .bind(channel)
    .bind(external_id)
    .fetch_optional(conn)
    .await?;
    Ok(row.map(|r| (r.get("conversation_id"), r.get("item_id"))))
}

/// The open case of a conversation (newest first) and its status.
pub async fn open_case(
    conn: &mut PgConnection,
    conversation_id: &str,
) -> ApiResult<Option<(String, String)>> {
    let row = sqlx::query(
        "SELECT case_id, status FROM agent_case \
          WHERE conversation_id = $1 AND status = ANY($2) \
          ORDER BY started_at DESC, case_id DESC LIMIT 1",
    )
    .bind(conversation_id)
    .bind(&OPEN_CASE[..])
    .fetch_optional(conn)
    .await?;
    Ok(row.map(|r| (r.get("case_id"), r.get("status"))))
}

/// The newest case of a conversation, open or closed.
pub async fn latest_case(
    conn: &mut PgConnection,
    conversation_id: &str,
) -> ApiResult<Option<String>> {
    Ok(sqlx::query_scalar(
        "SELECT case_id FROM agent_case WHERE conversation_id = $1 \
          ORDER BY started_at DESC, case_id DESC LIMIT 1",
    )
    .bind(conversation_id)
    .fetch_optional(conn)
    .await?)
}

/// The design a new case or handover item of `conv` runs on (D63; 0068 checks it): a `prod` conversation runs Live
/// (the job's current release, `None` = never released, its baseline), a `test` one the Draft at `jobs.draft_rev`.
/// Read in the writer's transaction.
pub async fn design_of(
    conn: &mut PgConnection,
    job_id: Uuid,
    job_key: &str,
    conv: &Conv,
) -> ApiResult<CaseDesign> {
    let env = if conv.env == "test" {
        design::Env::Test
    } else {
        design::Env::Production
    };
    let job = JobRef {
        id: job_id,
        key: job_key.to_owned(),
    };
    design::case_design(conn, &job, env).await
}

/// A design as the stored `(release_id, draft_rev)` of a case or handover item.
pub fn stamp(d: &CaseDesign) -> (Option<String>, Option<i64>) {
    match d {
        CaseDesign::Live { release_id } => (release_id.clone(), None),
        CaseDesign::Draft { draft_rev } => (None, Some(*draft_rev)),
    }
}

/// A new case (`CS-n`, status `open`) for `conv`, titled by the first message, with the conversation's party, and
/// stamped with `design` ([`design_of`], read in the same transaction).
pub async fn create_case(
    conn: &mut PgConnection,
    job_id: Uuid,
    conv: &Conv,
    first_text: &str,
    design: &CaseDesign,
) -> ApiResult<String> {
    let title: String = first_text.trim().chars().take(120).collect();
    let title = if title.is_empty() {
        "Conversation".to_string()
    } else {
        title
    };
    let (release_id, draft_rev) = stamp(design);
    Ok(sqlx::query_scalar(
        "INSERT INTO agent_case \
            (tenant_id, job_id, case_id, conversation_id, channel, env, release_id, draft_rev, title, status, \
             customer_id, customer_name, party_id, started_at, updated_at) \
         VALUES (app_current_tenant(), $1, agent_next_case_id(), $2, $3, $4, $5, $6, $7, 'open', $8, $9, \
                 (SELECT party_id FROM conversation WHERE conversation_id = $2), now(), now()) \
         RETURNING case_id",
    )
    .bind(job_id)
    .bind(&conv.id)
    .bind(&conv.channel)
    .bind(&conv.env)
    .bind(release_id)
    .bind(draft_rev)
    .bind(title)
    .bind(conv.customer_id.as_deref().unwrap_or(""))
    .bind(conv.customer_name.as_deref().unwrap_or(""))
    .fetch_one(conn)
    .await?)
}

/// The open case of `conv`, or a new one stamped with `design`.
pub async fn case_for(
    conn: &mut PgConnection,
    job_id: Uuid,
    conv: &Conv,
    text: &str,
    design: &CaseDesign,
) -> ApiResult<String> {
    match open_case(conn, &conv.id).await? {
        Some((id, _)) => Ok(id),
        None => create_case(conn, job_id, conv, text, design).await,
    }
}

/// Set a case's status (and stamp `resolved_at` on `resolved`).
pub async fn set_case_status(
    conn: &mut PgConnection,
    case_id: &str,
    status: &str,
    escalation_reason: Option<&str>,
) -> ApiResult<()> {
    sqlx::query(
        "UPDATE agent_case SET status = $2, \
                escalation_reason = COALESCE($3, escalation_reason), \
                resolved_at = CASE WHEN $2 = 'resolved' THEN now() ELSE resolved_at END, \
                updated_at = now() \
          WHERE case_id = $1",
    )
    .bind(case_id)
    .bind(status)
    .bind(escalation_reason)
    .execute(conn)
    .await?;
    Ok(())
}

/// A message to store.
pub struct NewMessage<'a> {
    pub job_id: Uuid,
    pub conversation_id: &'a str,
    /// `customer`, `agent`, `staff`, `system` or `note` (staff-only, never delivered).
    pub role: &'a str,
    pub author: &'a str,
    pub text: &'a str,
    pub case_id: Option<&'a str>,
    /// The provider's message id: a second message with the same id is not stored.
    pub external_id: Option<&'a str>,
}

/// A stored message.
#[derive(Clone, Debug)]
pub struct Stored {
    pub message_id: Uuid,
    pub at_ms: i64,
}

/// Has the conversation stored the provider message `external_id`?
pub async fn seen(
    conn: &mut PgConnection,
    conversation_id: &str,
    external_id: &str,
) -> ApiResult<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM conversation_message \
                         WHERE conversation_id = $1 AND external_id = $2)",
    )
    .bind(conversation_id)
    .bind(external_id)
    .fetch_one(conn)
    .await?)
}

/// Store `m`; `None` when its `external_id` was stored before (a provider retry).
pub async fn append(conn: &mut PgConnection, m: NewMessage<'_>) -> ApiResult<Option<Stored>> {
    append_client(conn, m, None, None).await
}

/// [`append`] with the sender's `client_msg_id` (a staff Send, SW T3) and the staff author's sub (`author_sub`, 0192:
/// who may deliver a failed reply again); `None` also when that id was stored before in the conversation (a retry of
/// the same Send).
pub async fn append_client(
    conn: &mut PgConnection,
    m: NewMessage<'_>,
    client_msg_id: Option<Uuid>,
    author_sub: Option<&str>,
) -> ApiResult<Option<Stored>> {
    let id = Uuid::now_v7();
    let at_ms = super::now_ms();
    let text: String = m.text.chars().take(MAX_TEXT).collect();
    let row: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO conversation_message \
            (tenant_id, job_id, conversation_id, message_id, role, author, text, case_id, refused, at_ms, external_id, \
             client_msg_id, author_sub) \
         VALUES (app_current_tenant(), $1, $2, $3, $4, $5, $6, $7, false, $8, $9, $10, $11) \
         ON CONFLICT DO NOTHING RETURNING message_id",
    )
    .bind(m.job_id)
    .bind(m.conversation_id)
    .bind(id)
    .bind(m.role)
    .bind(m.author)
    .bind(text)
    .bind(m.case_id)
    .bind(at_ms)
    .bind(m.external_id)
    .bind(client_msg_id)
    .bind(author_sub)
    .fetch_optional(conn)
    .await?;
    Ok(row.map(|message_id| Stored { message_id, at_ms }))
}

/// `MessageView` (SW T3): one message as the staff workspace reads it. `kind` is the role (`customer`, `agent`,
/// `staff`, `note`, `system`); `event` is a `system` row's subtype (its `author`: `agent_cases`, `resolution`, `csat`,
/// `hand_back`, `owner`, …), else `null`; `author_name` is the author of any other row; `at` is ISO-8601 UTC from
/// `at_ms`; `delivery` is a staff reply's (`sent`: stored, not confirmed by a channel; `delivered`; `failed`), else
/// `null`; `mirror` is a note's private comment on the helpdesk ticket (SW-11, 0206: `pending`, `sent`, `skipped` — no
/// ticket or binding to mirror to — or `failed`), `null` for a note that is not mirrored and for every other row.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct MessageView {
    pub message_id: Uuid,
    pub kind: String,
    pub event: Option<String>,
    pub text: String,
    pub author_name: Option<String>,
    pub at: String,
    pub case_id: Option<String>,
    pub client_msg_id: Option<Uuid>,
    pub delivery: Option<&'static str>,
    pub mirror: Option<&'static str>,
    /// SW-9: the files sent with the message (either direction); filled by [`with_attachments`], empty otherwise.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<crate::attachments::AttachmentRef>,
}

/// The `conversation_message` columns [`MessageView::of_row`] reads.
pub const MESSAGE_VIEW_COLS: &str = "message_id, role, author, text, case_id, at_ms, client_msg_id, delivered_at, \
     delivery_error, mirror_state";

impl MessageView {
    /// A row of [`MESSAGE_VIEW_COLS`].
    pub fn of_row(r: &sqlx::postgres::PgRow) -> Self {
        let role: String = r.get("role");
        let author: String = r.get::<Option<String>, _>("author").unwrap_or_default();
        let delivered: Option<chrono::DateTime<chrono::Utc>> = r.get("delivered_at");
        let failed: Option<String> = r.get("delivery_error");
        let delivery = (role == "staff").then_some(if failed.is_some() {
            "failed"
        } else if delivered.is_some() {
            "delivered"
        } else {
            "sent"
        });
        let mirror = match r.get::<Option<String>, _>("mirror_state").as_deref() {
            Some("pending") => Some("pending"),
            Some("sent") => Some("sent"),
            Some("skipped") => Some("skipped"),
            Some("failed") => Some("failed"),
            _ => None,
        };
        let (event, author_name) = if role == "system" {
            (Some(author), None)
        } else {
            (None, Some(author).filter(|a| !a.trim().is_empty()))
        };
        MessageView {
            message_id: r.get("message_id"),
            kind: role,
            event,
            text: r.get("text"),
            author_name,
            at: super::iso_ms(r.get("at_ms")),
            case_id: r.get("case_id"),
            client_msg_id: r.get("client_msg_id"),
            delivery,
            mirror,
            attachments: Vec::new(),
        }
    }
}

/// Fill the `attachments` of `views` (SW-9): the live attachments linked to each message, oldest first, as
/// [`AttachmentRef`](crate::attachments::AttachmentRef)s (never bytes, hash or uploader). `conn` is a job transaction.
pub async fn with_attachments(conn: &mut PgConnection, views: &mut [MessageView]) -> ApiResult<()> {
    if views.is_empty() {
        return Ok(());
    }
    let ids: Vec<Uuid> = views.iter().map(|v| v.message_id).collect();
    let rows = sqlx::query(
        "SELECT id, message_id, media_type, display_name, byte_size, width, height \
           FROM conversation_attachment \
          WHERE message_id = ANY($1) AND deleted_at IS NULL ORDER BY created_at, id",
    )
    .bind(&ids)
    .fetch_all(conn)
    .await?;
    for r in &rows {
        let message_id: Uuid = r.get("message_id");
        if let Some(v) = views.iter_mut().find(|v| v.message_id == message_id) {
            v.attachments.push(crate::attachments::AttachmentRef {
                id: r.get("id"),
                media_type: r.get("media_type"),
                display_name: r.get("display_name"),
                byte_size: r.get("byte_size"),
                width: r.get("width"),
                height: r.get("height"),
            });
        }
    }
    Ok(())
}

/// The [`MessageView`] of message `message_id`.
pub async fn message_view(conn: &mut PgConnection, message_id: Uuid) -> ApiResult<MessageView> {
    let r = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {MESSAGE_VIEW_COLS} FROM conversation_message WHERE message_id = $1"
    )))
    .bind(message_id)
    .fetch_one(&mut *conn)
    .await?;
    let mut view = MessageView::of_row(&r);
    with_attachments(conn, std::slice::from_mut(&mut view)).await?;
    Ok(view)
}

/// The message a Send with `client_msg_id` stored in the conversation, if any.
pub async fn message_by_client(
    conn: &mut PgConnection,
    conversation_id: &str,
    client_msg_id: Uuid,
) -> ApiResult<Option<MessageView>> {
    let r = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {MESSAGE_VIEW_COLS} FROM conversation_message \
          WHERE conversation_id = $1 AND client_msg_id = $2"
    )))
    .bind(conversation_id)
    .bind(client_msg_id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(r) = r else { return Ok(None) };
    let mut view = MessageView::of_row(&r);
    with_attachments(conn, std::slice::from_mut(&mut view)).await?;
    Ok(Some(view))
}

/// The last `limit` messages (staff notes included: a staff read), oldest first, as [`MessageView`]s with each one's
/// stored `author` and `at_ms` (the B12 fields the staff test chat still reads, crate::conversations::staff
/// `message_frame`).
pub async fn last_views(
    conn: &mut PgConnection,
    conversation_id: &str,
    limit: i64,
) -> ApiResult<Vec<(MessageView, String, i64)>> {
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT * FROM (SELECT {MESSAGE_VIEW_COLS} FROM conversation_message \
                         WHERE conversation_id = $1 ORDER BY at_ms DESC, message_id DESC LIMIT $2) t \
          ORDER BY at_ms, message_id"
    )))
    .bind(conversation_id)
    .bind(limit)
    .fetch_all(&mut *conn)
    .await?;
    let mut views: Vec<MessageView> = rows.iter().map(MessageView::of_row).collect();
    with_attachments(conn, &mut views).await?;
    Ok(rows
        .iter()
        .zip(views)
        .map(|(r, v)| {
            let author: String = r.get::<Option<String>, _>("author").unwrap_or_default();
            (v, author, r.get::<i64, _>("at_ms"))
        })
        .collect())
}

/// One stored message, as the history and the feeds read it.
#[derive(Clone, Debug)]
pub struct Msg {
    pub message_id: Uuid,
    pub role: String,
    pub author: String,
    pub text: String,
    pub case_id: Option<String>,
    pub at_ms: i64,
}

fn msg(r: &sqlx::postgres::PgRow) -> Msg {
    Msg {
        message_id: r.get("message_id"),
        role: r.get("role"),
        author: r.get::<Option<String>, _>("author").unwrap_or_default(),
        text: r.get("text"),
        case_id: r.get("case_id"),
        at_ms: r.get("at_ms"),
    }
}

/// The last `n` customer / caller / agent / staff turns, oldest first: what the model is given (a `caller` line is an
/// API-key caller's words, the requester's like a `customer` line). The stored handover notice
/// (an `agent` line by [`super::handover::NOTICE_AUTHOR`]) and the CSAT prompt ([`super::csat::PROMPT_AUTHOR`]) are
/// left out: the platform sent them, not the model, and fed back they would read as the model's own prior answers.
pub async fn history(
    conn: &mut PgConnection,
    conversation_id: &str,
    n: i64,
) -> ApiResult<Vec<Msg>> {
    let rows = sqlx::query(
        "SELECT * FROM (SELECT message_id, role, author, text, case_id, at_ms FROM conversation_message \
                         WHERE conversation_id = $1 AND role IN ('customer', 'caller', 'agent', 'staff') \
                           AND author IS DISTINCT FROM $3 AND author IS DISTINCT FROM $4 \
                         ORDER BY at_ms DESC, message_id DESC LIMIT $2) t \
          ORDER BY at_ms, message_id",
    )
    .bind(conversation_id)
    .bind(n)
    .bind(super::handover::NOTICE_AUTHOR)
    .bind(super::csat::PROMPT_AUTHOR)
    .fetch_all(conn)
    .await?;
    Ok(rows.iter().map(msg).collect())
}

/// The customer-facing messages (`customer`, `agent`, `staff`; never `system` lines or staff notes) after
/// `after_ms`, oldest first, at most `limit`.
pub async fn messages_after(
    conn: &mut PgConnection,
    conversation_id: &str,
    after_ms: i64,
    limit: i64,
) -> ApiResult<Vec<Msg>> {
    let rows = sqlx::query(
        "SELECT message_id, role, author, text, case_id, at_ms FROM conversation_message \
          WHERE conversation_id = $1 AND at_ms > $2 AND role IN ('customer', 'agent', 'staff') \
          ORDER BY at_ms, message_id LIMIT $3",
    )
    .bind(conversation_id)
    .bind(after_ms)
    .bind(limit)
    .fetch_all(conn)
    .await?;
    Ok(rows.iter().map(msg).collect())
}

/// The last `limit` messages that may leave the platform (every role but `note`), oldest first: a worker channel's
/// excerpt of a new handover item (SW-11: notes are never mirrored).
pub async fn last_shared(
    conn: &mut PgConnection,
    conversation_id: &str,
    limit: i64,
) -> ApiResult<Vec<Msg>> {
    let rows = sqlx::query(
        "SELECT * FROM (SELECT message_id, role, author, text, case_id, at_ms FROM conversation_message \
                         WHERE conversation_id = $1 AND role <> 'note' \
                         ORDER BY at_ms DESC, message_id DESC LIMIT $2) t \
          ORDER BY at_ms, message_id",
    )
    .bind(conversation_id)
    .bind(limit)
    .fetch_all(conn)
    .await?;
    Ok(rows.iter().map(msg).collect())
}

/// The last `limit` messages (staff notes included: a staff read), oldest first.
pub async fn last_messages(
    conn: &mut PgConnection,
    conversation_id: &str,
    limit: i64,
) -> ApiResult<Vec<Msg>> {
    let rows = sqlx::query(
        "SELECT * FROM (SELECT message_id, role, author, text, case_id, at_ms FROM conversation_message \
                         WHERE conversation_id = $1 ORDER BY at_ms DESC, message_id DESC LIMIT $2) t \
          ORDER BY at_ms, message_id",
    )
    .bind(conversation_id)
    .bind(limit)
    .fetch_all(conn)
    .await?;
    Ok(rows.iter().map(msg).collect())
}

/// The advisory-lock key every owner change of a conversation takes first (Take / Assign / Hand back, console
/// reply; crate::conversations::ownership), as the runtime's drain and reply paths do.
pub fn control_key(conversation_id: &str) -> String {
    format!("control:{conversation_id}")
}

/// The SQL name of a `users` row: the display name, else the email, else the sub (B13 §4).
pub const MEMBER_NAME_SQL: &str =
    "coalesce(nullif(btrim(display_name), ''), nullif(btrim(email), ''), sub)";

/// The display name of a member ([`MEMBER_NAME_SQL`]); not a member: the name it last presented as staff (0115
/// `actor_names`, `crate::members::names::actor_seen`), else the sub.
pub async fn member_name(conn: &mut PgConnection, sub: &str) -> ApiResult<String> {
    let n: Option<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT coalesce((SELECT {MEMBER_NAME_SQL} FROM users WHERE sub = $1), agent_actor_name($1))"
    )))
    .bind(sub)
    .fetch_one(conn)
    .await?;
    Ok(n.unwrap_or_else(|| sub.to_string()))
}

/// The job's key (the job must exist in the transaction's tenant), else 404 `job_not_found`.
pub async fn job_key(conn: &mut PgConnection, job_id: Uuid) -> ApiResult<String> {
    sqlx::query_scalar("SELECT key FROM jobs WHERE id = $1")
        .bind(job_id)
        .fetch_optional(conn)
        .await?
        .ok_or_else(|| ApiError::Missing("job_not_found", "No such job.".into()))
}

/// Text a channel may carry: no NUL and no other C0 control except tab, newline and carriage return (Postgres refuses
/// NUL in text and jsonb; the rest is never legitimate chat content).
pub fn clean(s: &str) -> bool {
    !s.chars()
        .any(|c| (c < ' ' && !matches!(c, '\t' | '\n' | '\r')) || c == '\u{7f}')
}

/// 422 `invalid_body` unless every string is [`clean`].
pub fn require_clean<'a>(fields: impl IntoIterator<Item = &'a str>) -> ApiResult<()> {
    if fields.into_iter().all(clean) {
        Ok(())
    } else {
        Err(ApiError::Unprocessable(
            "invalid_body",
            "The request carries control characters.".into(),
        ))
    }
}

/// 422 `invalid_body` unless every string (keys included) of `v` is [`clean`].
pub fn require_clean_json(v: &serde_json::Value) -> ApiResult<()> {
    fn ok(v: &serde_json::Value) -> bool {
        match v {
            serde_json::Value::String(s) => clean(s),
            serde_json::Value::Array(a) => a.iter().all(ok),
            serde_json::Value::Object(m) => m.iter().all(|(k, v)| clean(k) && ok(v)),
            _ => true,
        }
    }
    if ok(v) {
        Ok(())
    } else {
        Err(ApiError::Unprocessable(
            "invalid_body",
            "The request carries control characters.".into(),
        ))
    }
}

/// A stored provider message: its case, and whether anything (agent, staff, system; never a staff note) followed it.
pub async fn retry_state(
    conn: &mut PgConnection,
    conversation_id: &str,
    external_id: &str,
) -> ApiResult<Option<(Option<String>, bool)>> {
    let row = sqlx::query(
        "SELECT m.case_id, EXISTS (SELECT 1 FROM conversation_message n \
                                     WHERE n.conversation_id = m.conversation_id \
                                       AND n.role NOT IN ('customer', 'note') \
                                       AND (n.at_ms, n.message_id) > (m.at_ms, m.message_id)) AS followed \
           FROM conversation_message m WHERE m.conversation_id = $1 AND m.external_id = $2",
    )
    .bind(conversation_id)
    .bind(external_id)
    .fetch_optional(conn)
    .await?;
    Ok(row.map(|r| (r.get("case_id"), r.get("followed"))))
}

/// The author of the newest customer-facing message (`customer`, `agent`, `staff`; never `system` or `note`), with
/// its role.
pub async fn last_outbound(
    conn: &mut PgConnection,
    conversation_id: &str,
) -> ApiResult<Option<(String, String)>> {
    let row = sqlx::query(
        "SELECT role, author FROM conversation_message \
          WHERE conversation_id = $1 AND role IN ('customer', 'agent', 'staff') \
          ORDER BY at_ms DESC, message_id DESC LIMIT 1",
    )
    .bind(conversation_id)
    .fetch_optional(conn)
    .await?;
    Ok(row.map(|r| (r.get("role"), r.get("author"))))
}
