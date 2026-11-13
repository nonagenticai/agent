//! The staff test chat's writes (B4 T9, plan R4): `startChat` and `sendChatMessage` (web `areas/live.ts`). Any member
//! (route table "Chat start and send: any member"; tests/roles.rs lists both as conversation routes).
//!
//! - `POST /api/jobs/{job}/conversations` `{env, customer_email}` → 201 `ConversationStart`. `prod` needs a published
//!   release (409 `no_release`); a customer job needs the test customer's email from the staff member (422
//!   `test_customer_required` / `invalid_test_customer`): the conversation is a known customer's (`identity_source`
//!   `staff_test`), resolved through the job's real connectors. Without the
//!   runtime (`REDIS_URL` unset) it is 503 `unavailable`, the send's answer, and stores nothing. A tenant that is not
//!   active (suspended: a super_admin may still act in it) is 409 `tenant_not_active` with the customer's neutral
//!   text, and stores nothing (crate::turn::admit_start, the drain's own gate 2).
//! - `POST /api/jobs/{job}/conversations/{id}/messages` `{text}` → **202** `{status, conversation_id, message_id}`
//!   right after the message is stored; the reply arrives on `GET …/conversations/{id}/events`. 422 `empty_message` /
//!   `message_too_long`; 409 `conversation_ended`; 409 `conversation_busy` (nothing stored) while a turn runs. The
//!   send goes through the one channel-agnostic path, crate::turn::accept_customer_message.
//! - The same path with a `kind` in the body (`{kind: "reply" | "note", text, client_msg_id}`) is the Staff Workspace
//!   Send (SW T3, crate::conversations::routes::send_message) on any channel's conversation; it checks its own role by
//!   kind.
//!
//! `chatConversation` (`GET …/conversations/{id}`) is B12's transcript route (crate::conversations::routes).
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Json;
use axum::routing::post;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use super::chat_profile::{self, TestParty};
use crate::conversations::{iso, store};
use crate::error::{ApiError, ApiJson, ApiPath, ApiResult};
use crate::interaction::Scope;
use crate::state::AppState;
use crate::tenancy::JobMember;
use crate::turn::{self, Inbound};

/// The longest staff chat message (characters; the web's limit).
pub const MAX_CHAT_TEXT: usize = 2_000;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/jobs/{job}/conversations", post(start_chat))
        .route(
            "/api/jobs/{job}/conversations/{id}/messages",
            post(send_chat_message),
        )
}

#[derive(Deserialize)]
struct StartBody {
    env: String,
    /// The test customer's email the STAFF MEMBER configures the test with (required for a customer job; 422
    /// `test_customer_required`). IGNORED for a staff-audience job: its "customer" is the signed-in employee, bound
    /// from the verified token, never from here.
    #[serde(default)]
    customer_email: String,
}

/// `startChat`.
async fn start_chat(
    JobMember(j): JobMember,
    State(st): State<AppState>,
    ApiJson(b): ApiJson<StartBody>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    store::require_clean([b.env.as_str(), b.customer_email.as_str()])?;
    let env = match b.env.as_str() {
        "prod" => "prod",
        "test" => "test",
        _ => {
            return Err(ApiError::Unprocessable(
                "invalid_env",
                "env is prod or test.".into(),
            ));
        }
    };
    // a staff-audience job (IT desk, HR): the "customer" is the signed-in employee, known by the token's VERIFIED
    // email alone (never the body, a slot or the model); an unverified email opens nothing
    let staff_party = staff_identity(&st, &j).await?;
    // a customer job: the staff member names the test customer (their email) before anything is stored
    let test_email = if staff_party.is_some() {
        None
    } else {
        Some(test_customer_email(&b.customer_email)?)
    };
    // without the runtime (`REDIS_URL` unset) no message could ever be sent: refuse before storing a conversation,
    // with the send's own 503
    if st.redis.is_none() {
        return Err(turn::unavailable());
    }
    // gate 2 at the start (A10): a tenant whose turns are refused opens no conversation either (a super_admin may
    // act in a suspended tenant); 409 `tenant_not_active`, nothing stored
    turn::admit_start(&st, j.t.tenant, &j.job_key, "staff").await?;
    if env == "prod" {
        let mut tx = j.begin(&st.db).await?;
        let released: bool =
            sqlx::query_scalar("SELECT current_release_id IS NOT NULL FROM jobs WHERE id = $1")
                .bind(j.job_id)
                .fetch_one(&mut *tx)
                .await?;
        tx.commit().await?;
        if !released {
            return Err(ApiError::Conflict(
                "no_release",
                "This job has no release yet — nothing is live. Talk to the Draft (Test env), then publish."
                    .into(),
            ));
        }
    }
    let (customer, identity_source, customer_email) = match staff_party {
        Some(email) => (
            TestParty {
                customer_id: email.clone(),
                name: j.t.caller.actor.clone(),
            },
            "assertion",
            Some(email),
        ),
        // a staff test over a customer job: the typed email is the known customer (customer_id + KYC email). The
        // system keys its customers by its own id, so the context phase first resolves the email to that party
        // (`party.list{email}`, `runtime::customer_context`), binds the case to it, then reads the records through the
        // job's real connectors exactly as for a verified web chat; no match is a degraded context, never a question
        None => {
            let email = test_email.unwrap_or_default();
            (
                TestParty {
                    customer_id: email.clone(),
                    name: email.clone(),
                },
                "staff_test",
                Some(email),
            )
        }
    };
    if env == "test" {
        warm_draft(&st, &j).await;
    }
    let id = format!("conv-{}", Uuid::now_v7());
    let mut tx = j.begin(&st.db).await?;
    let started: chrono::DateTime<chrono::Utc> = sqlx::query_scalar(
        "INSERT INTO conversation \
            (tenant_id, job_id, conversation_id, env, channel, customer_id, initiator_sub, started_at) \
         VALUES (app_current_tenant(), $1, $2, $3, 'staff', $4, $5, now()) \
         RETURNING started_at",
    )
    .bind(j.job_id)
    .bind(&id)
    .bind(env)
    .bind(&customer.customer_id)
    .bind(&j.t.caller.sub)
    .fetch_one(&mut *tx)
    .await?;
    // the staff-chosen customer as conversation state (staff_pick: the staff member's own party; staff_test: the
    // typed test customer)
    crate::conversations::state::put_known_identity(
        &mut tx,
        &id,
        match identity_source {
            "assertion" => crate::conversations::state::SOURCE_STAFF_PICK,
            other => other,
        },
        &customer.customer_id,
        customer_email.as_deref(),
        Some(customer.name.as_str()),
        &format!("staff:{}", j.t.caller.sub),
    )
    .await?;
    // SW-1: every conversation has an owner row, the agent's from the start
    crate::conversations::ownership::set(
        &mut tx,
        &id,
        &crate::conversations::ownership::Owner::Agent,
    )
    .await?;
    tx.commit().await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "conversation_id": id,
            "customer_id": customer.customer_id,
            "customer_name": customer.name,
            "env": env,
            "channel": "staff",
            "started_at": iso(started),
        })),
    ))
}

/// Warm the draft's platform-pack cache before a test-chat turn (the runtime only reads it; see
/// `resolve::warm_draft`). Best effort.
async fn warm_draft(st: &AppState, j: &crate::tenancy::JobCtx) {
    let jr = crate::design::JobRef {
        id: j.job_id,
        key: j.job_key.clone(),
    };
    crate::knowledge::packs::resolve::warm_draft(st, j.t.tenant, &jr).await;
}

/// The test customer's email: trimmed, lower-cased, `local@domain.tld`. Missing is 422 `test_customer_required`, a
/// malformed one 422 `invalid_test_customer`.
fn test_customer_email(raw: &str) -> ApiResult<String> {
    let email = raw.trim().to_lowercase();
    if email.is_empty() {
        return Err(ApiError::Unprocessable(
            "test_customer_required",
            "Enter the email of the customer to test as.".into(),
        ));
    }
    if email.len() > 320 || !crate::audit::scrub::looks_like_email(&email) {
        return Err(ApiError::Unprocessable(
            "invalid_test_customer",
            "The test customer's email is not a valid address.".into(),
        ));
    }
    Ok(email)
}

/// The employee's party for a staff-audience job: `None` for a customer-audience job; the caller's token email
/// (lower-cased, `email_verified` only) for a staff one. An unverified or missing email is 403
/// `email_not_verified` and nothing is stored.
async fn staff_identity(st: &AppState, j: &crate::tenancy::JobCtx) -> ApiResult<Option<String>> {
    let mut tx = j.begin(&st.db).await?;
    let audience = chat_profile::audience_of(&mut tx, j.job_id).await?;
    tx.commit().await?;
    if audience != "staff" {
        return Ok(None);
    }
    // `Caller.verified_email` is the token's email only when `email_verified`; no other source is read
    crate::runtime::customer_context::staff_party(j.t.caller.verified_email.as_deref(), true)
        .map(Some)
        .ok_or_else(|| {
            ApiError::Forbidden(
                "email_not_verified",
                "Sign in with a verified email address to chat as an employee.".into(),
            )
        })
}

#[derive(Deserialize)]
struct SendBody {
    #[serde(default)]
    text: String,
    /// SW T3 Send: `reply` | `note`; absent for the staff test chat.
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    client_msg_id: Option<Uuid>,
    /// SW-9: ids of uploaded attachments a reply carries (at most 5).
    #[serde(default)]
    attachments: Vec<Uuid>,
}

/// `sendChatMessage`: 202 once the message is stored and queued; the turn runs after the answer. With a `kind`: the
/// Staff Workspace Send (crate::conversations::routes::send_message).
async fn send_chat_message(
    JobMember(j): JobMember,
    State(st): State<AppState>,
    ApiPath((_, id)): ApiPath<(String, String)>,
    ApiJson(b): ApiJson<SendBody>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    if let Some(kind) = b.kind.as_deref() {
        return crate::conversations::routes::send_message(
            &st,
            &j,
            &id,
            kind,
            &b.text,
            b.client_msg_id,
            b.attachments,
        )
        .await;
    }
    store::require_clean([b.text.as_str()])?;
    let text = b.text.trim();
    if text.is_empty() {
        return Err(ApiError::Unprocessable(
            "empty_message",
            "Type a message first.".into(),
        ));
    }
    if text.chars().count() > MAX_CHAT_TEXT {
        return Err(ApiError::Unprocessable(
            "message_too_long",
            "Messages are at most 2,000 characters.".into(),
        ));
    }
    let conv = {
        let mut tx = j.begin(&st.db).await?;
        let conv = store::conversation_or_404(&mut tx, &id).await?;
        tx.commit().await?;
        conv
    };
    // this door serves the staff test chat only; a customer channel's conversation has its own
    if conv.channel != "staff" {
        return Err(ApiError::Missing(
            "conversation_not_found",
            "No such conversation in this job.".into(),
        ));
    }
    if conv.env == "test" {
        warm_draft(&st, &j).await;
    }
    let author = conv
        .customer_name
        .clone()
        .or_else(|| conv.customer_id.clone())
        .unwrap_or_else(|| "customer".into());
    let scope = Scope::from(&j);
    let accepted = turn::accept_message(
        &st,
        Inbound {
            scope: &scope,
            conv: &conv,
            author: &author,
            actor: j.t.actor(),
            text,
            external_id: None,
        },
        turn::AuthorRole::User,
    )
    .await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({
            "status": 202,
            "conversation_id": conv.id,
            "message_id": accepted.message_id.to_string(),
        })),
    ))
}
