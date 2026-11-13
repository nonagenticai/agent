//! agent-edge → agent-api (B12 §3): `/api/internal/channel/*`, gated by [`EdgeKey`] (`Authorization: Bearer
//! $EDGE_INTERNAL_KEY`, compared in constant time; unset → 503 `edge_not_configured`, mismatch → 401). No person, no
//! tenant role: the edge names the tenant and job (resolved from a binding), and every write runs in
//! [`store::begin_job`] for that scope. Not routed by the public VirtualService.
//!
//! - `GET  /api/internal/channel/bindings` — every enabled binding of every tenant, credential opened.
//! - `POST /api/internal/channel/inbound` — one customer message: answered, queued, rated or a duplicate. With
//!   `accept_only` (B7 R6 persist-before-200) the message is stored and deduped but not answered: `accepted` where
//!   the agent would answer; the edge's second call without the flag answers it (the provider-retry path below).
//!   D109 (KH T6): the agent's answer is always a B4 runtime turn — 409 `job_not_live` while the job has no live
//!   release, 503 `unavailable` while no runtime process serves the scope; there is no model fallback.
//! - `POST /api/internal/channel/handover/notice-delivered` — the edge delivered an item's handover notice (R6): a
//!   resent accept of the queuing message is then `duplicate`, so the customer is told once across edge pods. 204.
//! - `POST /api/internal/channel/staff` — a reply in a worker thread (Frappe Helpdesk): the staff
//!   member takes the conversation (crate::conversations::ownership::take_scoped: 409 `owner_changed` while a
//!   colleague owns it, 409 `closed` in a closed item's thread), then replies through the one Send path
//!   (crate::conversations::staff::reply). SW-5: what they do there shows in the workspace, and the reverse.
//! - `POST /api/internal/channel/staff-state` — staff closed the handover in their own tool (a Frappe Helpdesk ticket
//!   Resolved / Closed, B7 R9): the same Take, then the one Resolve (crate::conversations::ownership::resolve_scoped,
//!   outcome `resolved`): the item and its case go to resolved, the agent owns the conversation again, the customer
//!   is told; idempotent.
//! - `GET  /api/internal/channel/conversations/{conversation_id}/messages?after_ms=` — web chat reconnect.
//! - `POST /api/internal/channel/webchat/sessions` — a web chat session (crate::conversations::webchat_sessions, B13).
//! - `GET  /api/internal/channel/webchat/conversations/{conversation_id}/events` — the web chat customer stream the
//!   edge proxies (crate::conversations::webchat_stream).
//! - `POST /api/internal/channel/webhook-status` — the edge's observed Telegram webhook health of one binding (B7
//!   T15b): the binding is checked like `inbound` (kind, customer side, tenant, job), then `channel_webhook_status`
//!   is upserted, merged across replicas (crate::channels::webhook_health::degraded_since / status_of).
use axum::Router;
use axum::extract::{FromRequestParts, State};
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::response::Json;
use axum::routing::{get, post};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::Row;
use uuid::Uuid;

use super::handover::{self, State as HoState};
use super::ownership;
use super::staff::{self, MsgKind, Reply};
use super::{CHANNELS, answer_sweeper, bindings, csat, human, store};
use crate::audit::{self, Actor, Event};
use crate::auth::TenantStatus;
use crate::channels::connector::Side;
use crate::channels::core::{
    BindingsResp, CoreBinding, HistoryMessage, HistoryResp, InboundReq, InboundResp, InternalKey,
    NoticeDeliveredReq, StaffReq, StaffResp, StaffStateReq, StaffStateResp, StaffStateStatus,
    StaffStatus, WebhookInfoReport, WebhookStatusReq, WebhookStatusResp,
};
use crate::channels::webhook_health;
use crate::error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult};
use crate::interaction::{ConversationRef, Scope};
use crate::runtime::backend::{self as runtime_backend, Outcome, RuntimeBackend};
use crate::state::AppState;
use crate::turn::MessageState;

/// The internal door's guard (see the module doc): the edge's own [`InternalKey::verify`] (HMAC tag comparison,
/// constant time).
pub struct EdgeKey;

impl FromRequestParts<AppState> for EdgeKey {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, st: &AppState) -> Result<Self, ApiError> {
        let Some(key) = st
            .cfg
            .edge
            .internal_key
            .as_deref()
            .and_then(InternalKey::new)
        else {
            return Err(ApiError::Unavailable(
                "edge_not_configured",
                "The channel edge is not configured.".into(),
                None,
            ));
        };
        if !key.verify(&parts.headers) {
            return Err(ApiError::Unauthenticated);
        }
        Ok(EdgeKey)
    }
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/internal/channel/bindings", get(internal_bindings))
        .route("/api/internal/channel/inbound", post(inbound))
        .route(
            "/api/internal/channel/handover/notice-delivered",
            post(notice_delivered),
        )
        .route("/api/internal/channel/staff", post(staff_message))
        .route("/api/internal/channel/staff-state", post(staff_state))
        .route("/api/internal/channel/webhook-status", post(webhook_status))
        .route(
            "/api/internal/channel/conversations/{conversation_id}/messages",
            get(conversation_messages),
        )
        .merge(super::webchat_sessions::routes())
        .merge(super::webchat_stream::routes())
        .merge(super::webchat_attachments::routes())
}

/// One enabled binding of any tenant (`agent_connector_bindings()`).
struct Binding {
    id: Uuid,
    tenant: Uuid,
    job_id: Uuid,
    job_key: String,
    kind: String,
    side: String,
    row: sqlx::postgres::PgRow,
}

async fn all_bindings(st: &AppState) -> ApiResult<Vec<Binding>> {
    let rows = sqlx::query(
        "SELECT id, tenant_id, job_id, job_key, env, kind, side, external_id, label, settings, \
                secret_enc, secret_nonce, secret_dek_version FROM agent_connector_bindings()",
    )
    .fetch_all(&st.db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| Binding {
            id: r.get("id"),
            tenant: r.get("tenant_id"),
            job_id: r.get("job_id"),
            job_key: r.get("job_key"),
            kind: r.get("kind"),
            side: r.get("side"),
            row: r,
        })
        .collect())
}

async fn internal_bindings(
    _k: EdgeKey,
    State(st): State<AppState>,
) -> ApiResult<Json<BindingsResp>> {
    let mut out = Vec::new();
    for b in all_bindings(&st).await? {
        let r = &b.row;
        let sealed = (
            r.get::<Option<Vec<u8>>, _>("secret_enc"),
            r.get::<Option<Vec<u8>>, _>("secret_nonce"),
            r.get::<Option<i32>, _>("secret_dek_version"),
        );
        let credential = if sealed.0.is_some() {
            let mut tx = crate::tenancy::begin_scoped(&st.db, b.tenant).await?;
            let c = bindings::open_credential(&mut tx, &st, b.tenant, b.id, sealed).await;
            tx.commit().await?;
            c.and_then(|v| match v {
                Value::Object(m) => Some(
                    m.into_iter()
                        .map(|(k, v)| match v {
                            Value::String(x) => (k, x),
                            other => (k, other.to_string()),
                        })
                        .collect(),
                ),
                _ => None,
            })
        } else {
            None
        };
        out.push(CoreBinding {
            id: b.id,
            kind: b.kind.clone(),
            side: if b.side == "worker" {
                Side::Worker
            } else {
                Side::Customer
            },
            external_id: r.get("external_id"),
            tenant: b.tenant,
            job_id: b.job_id,
            job_key: b.job_key.clone(),
            env: r.get("env"),
            settings: r.get("settings"),
            credential,
        });
    }
    Ok(Json(BindingsResp { bindings: out }))
}

/// The tenant must exist and be active (404 `tenant_not_found`, 409 `tenant_not_active`). Not active is gate 2 at
/// the start ([`crate::turn::admit_start`], A10): the customer's neutral text, audited once per tenant per hour with
/// the drain's refusals, and no conversation is upserted (this runs before any write).
async fn active_tenant(st: &AppState, tenant: Uuid, job_key: &str, kind: &str) -> ApiResult<()> {
    match crate::tenancy::tenant_status(&st.db, tenant).await? {
        None => Err(ApiError::Missing(
            "tenant_not_found",
            "No such tenant.".into(),
        )),
        Some(TenantStatus::Active) => Ok(()),
        Some(_) => crate::turn::admit_start(st, tenant, job_key, kind).await,
    }
}

fn check_text(text: &str) -> ApiResult<String> {
    let t = text.trim();
    if t.is_empty() {
        return Err(ApiError::Unprocessable(
            "empty_message",
            "The message is empty.".into(),
        ));
    }
    if t.chars().count() > store::MAX_TEXT {
        return Err(ApiError::Unprocessable(
            "message_too_long",
            "The message is too long.".into(),
        ));
    }
    Ok(t.to_string())
}

fn check_ids(conversation: &ConversationRef, message_id: &str, party: &str) -> ApiResult<()> {
    let bad =
        |s: &str| s.trim().is_empty() || s.chars().count() > 512 || s.chars().any(char::is_control);
    if bad(&conversation.id) || bad(message_id) || bad(party) {
        return Err(ApiError::Unprocessable(
            "invalid_reference",
            "conversation.id, message_id and party.id are 1 to 512 characters".into(),
        ));
    }
    Ok(())
}

/// Every string of a conversation ref (id, route keys and values).
fn ref_strings(r: &ConversationRef) -> impl Iterator<Item = &str> {
    std::iter::once(r.id.as_str())
        .chain(std::iter::once(r.channel.as_str()))
        .chain(r.route.iter().flat_map(|(k, v)| [k.as_str(), v.as_str()]))
}

/// One enabled, verified binding (`agent_connector_bindings()`), checked against what the edge says about it:
/// kind, side, scope and (when given) the account. Any mismatch is the same 404 `binding_not_found`.
pub(super) async fn checked_binding(
    st: &AppState,
    id: Uuid,
    kind: &str,
    side: &str,
    tenant: Uuid,
    job_id: Uuid,
    external_id: Option<&str>,
) -> ApiResult<String> {
    checked_binding_env(st, id, kind, side, tenant, job_id, external_id)
        .await
        .map(|(job_key, _)| job_key)
}

/// [`checked_binding`] with the binding's env (D130: `test` until the channel is published, then `prod`).
pub(super) async fn checked_binding_env(
    st: &AppState,
    id: Uuid,
    kind: &str,
    side: &str,
    tenant: Uuid,
    job_id: Uuid,
    external_id: Option<&str>,
) -> ApiResult<(String, String)> {
    let row = sqlx::query(
        "SELECT job_key, env, kind, side, external_id, tenant_id, job_id FROM agent_connector_bindings() WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(&st.db)
    .await?;
    let ok = row.as_ref().is_some_and(|r| {
        r.get::<String, _>("kind") == kind
            && r.get::<String, _>("side") == side
            && r.get::<Uuid, _>("tenant_id") == tenant
            && r.get::<Uuid, _>("job_id") == job_id
            && external_id.is_none_or(|e| r.get::<String, _>("external_id") == e)
    });
    match row {
        Some(r) if ok => Ok((r.get("job_key"), r.get("env"))),
        _ => Err(ApiError::Missing(
            "binding_not_found",
            "No enabled, verified binding matches this message.".into(),
        )),
    }
}

async fn inbound(
    _k: EdgeKey,
    State(st): State<AppState>,
    ApiJson(b): ApiJson<InboundReq>,
) -> ApiResult<Json<InboundResp>> {
    // SW-6: a CSAT request this message dismissed is counted only when the message was handled (its transaction
    // committed), so a rolled-back attempt that the edge retries is not counted twice
    let mut dismissed = Vec::new();
    let out = inbound_message(st, b, &mut dismissed).await;
    if out.is_ok() {
        dismissed.into_iter().for_each(csat::count_dismissed);
    }
    out
}

/// [`inbound`]'s work; `dismissed` collects the CSAT dismissals it made.
async fn inbound_message(
    st: AppState,
    b: InboundReq,
    dismissed: &mut Vec<csat::Dismissal>,
) -> ApiResult<Json<InboundResp>> {
    let kind = b.kind.as_str();
    if !CHANNELS.contains(&kind) || kind == "staff" {
        return Err(ApiError::Unprocessable(
            "invalid_channel",
            "kind is not a customer channel".into(),
        ));
    }
    store::require_clean(
        [
            b.text.as_str(),
            b.message_id.as_str(),
            b.party.id.as_str(),
            b.party.name.as_deref().unwrap_or_default(),
            b.external_id.as_str(),
        ]
        .into_iter()
        .chain(ref_strings(&b.conversation)),
    )?;
    let text = check_text(&b.text)?;
    check_ids(&b.conversation, &b.message_id, &b.party.id)?;
    let attach_ids = super::webchat_attachments::checked_ids(kind, &b.attachments)?;
    active_tenant(&st, b.tenant, &b.job_key, kind).await?;
    let (job_key, env) = checked_binding_env(
        &st,
        b.binding_id,
        kind,
        "customer",
        b.tenant,
        b.job_id,
        Some(&b.external_id),
    )
    .await?;
    // B13 §3: web chat on a core binding needs its session (`session_token`, party `wc:<session_id>`), then the
    // durable limits and abuse rules; fail closed (503) when the database cannot answer
    let wc_session = if kind == "web_chat" {
        Some(super::webchat_sessions::admit(&st, &b, &job_key, &text).await?)
    } else {
        None
    };
    let scope = Scope {
        tenant: b.tenant,
        job_id: b.job_id,
        job_key,
    };
    let mut cref = b.conversation.clone();
    cref.channel = kind.to_string();
    cref.route
        .insert("binding_id".into(), b.binding_id.to_string());
    // D109: the runtime is the only thing that answers a customer (a live `interaction` process serves this scope),
    // read before any lock is taken; `None` = 503 `unavailable` where the agent would answer
    let runtime = RuntimeBackend::serving(&st, &scope, &env).await;
    let mut tx = store::begin_job(&st.db, b.tenant, b.job_id).await?;
    // B13 §3: a session that already has its conversation keeps it, whatever ref the edge sent (a first message
    // resent before the widget held its conversation token): its own customer ref replaces the presented one
    if let Some(own) = wc_session
        .as_ref()
        .and_then(|s| s.conversation_id.as_deref())
        && let Some((r, _)) = store::customer_ref(&mut tx, own).await?
        && r.channel == kind
    {
        cref.id = r.id;
    }
    store::lock(
        &mut tx,
        &format!("{}:{}:{kind}:{}", b.tenant, b.job_id, cref.id),
    )
    .await?;
    // SW T5: the conversation's control lock and owner row FIRST (ownership::guard), before this transaction writes
    // or locks any row of it: the ref, the session, and above all `party::attach`, which updates the conversation's
    // `agent_case` rows. A Take holds the control lock and then updates its case (`set_case_status`); taken after
    // `party::attach` (as at 836f086f) the two deadlocked (40P01, the Take's 500 in the race test). A Take running
    // now either committed first (its owner and item are read below) or waits for this one; a "talk to a person"
    // below queues its item in the commands' lock order. A conversation that does not exist yet is created below
    // (nobody can Take it before this commits) and guarded right after.
    let known: Option<String> = sqlx::query_scalar(
        "SELECT conversation_id FROM channel_conversation_refs \
          WHERE channel = $1 AND side = 'customer' AND external_id = $2",
    )
    .bind(kind)
    .bind(&cref.id)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(id) = &known {
        ownership::guard(&mut tx, id).await?;
    }
    let conv = store::upsert_customer_conversation(
        &mut tx,
        b.job_id,
        kind,
        &cref,
        &b.party.id,
        b.party.name.as_deref(),
        &env,
    )
    .await?;
    // taken twice in one transaction it changes nothing (the ref's conversation is this one)
    let own = ownership::guard(&mut tx, &conv.id).await?;
    // B13 §3: one web chat session is one conversation
    if let Some(s) = &wc_session {
        super::webchat_sessions::attach(&mut tx, s, &conv.id).await?;
    }
    // B13 §1: the party behind the wire subject (the binding's kind is the channel), on the conversation and its rows
    // a web chat identity comes ONLY from the session row (`Admitted.customer`, set when its assertion verified),
    // never from the wire: the edge's `party.verified` is not trusted for a session-backed web chat message
    let known_customer = wc_session.as_ref().and_then(|s| s.customer.as_ref());
    let verified = if wc_session.is_some() {
        known_customer.is_some()
    } else {
        b.party.verified
    };
    let party_id = crate::party::resolve(
        &mut tx,
        b.tenant,
        kind,
        &b.party.id,
        b.party.name.as_deref(),
        verified,
    )
    .await?;
    crate::party::attach(&mut tx, &conv.id, party_id).await?;
    if let Some(kc) = known_customer {
        // a staff-audience job (the IT desk's employee chat) binds the employee by verified email, not the assertion's sub
        let staff = crate::routes::chat_profile::audience_of(&mut tx, b.job_id).await? == "staff";
        bind_known_customer(&mut tx, &conv.id, &b.party.id, kc, staff).await?;
    }
    // SW-1: anyone but the agent owns the conversation (the queue or a staff member): a person answers
    let silenced = own.is_some_and(|o| ownership::silenced(&o.owner));
    let open_item = handover::open_for_conversation(&mut tx, &conv.id).await?;
    // a provider retry, or R6's answer call for a message its accept stored. Where the runtime tracked the message
    // (`turn::MessageState`), its own turn decides: answered → duplicate, waiting → its reply, never queued → queued
    // now. Otherwise (no state in Redis): answered or with a person → duplicate, else its turn runs now.
    if let Some((case_id, followed)) = store::retry_state(&mut tx, &conv.id, &b.message_id).await? {
        let duplicate = Json(InboundResp::Duplicate {
            conversation_id: Some(conv.id.clone()),
        });
        // SW-9: the stored message (a first attempt, or an `accept_only` call) and the files this call names: linked
        // here, also when a person owns the conversation (best effort: files already linked, or none, change nothing)
        let stored: Option<Uuid> = sqlx::query_scalar(
            "SELECT message_id FROM conversation_message \
              WHERE conversation_id = $1 AND external_id = $2 AND role = 'customer'",
        )
        .bind(&conv.id)
        .bind(&b.message_id)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(mid) = stored
            && !attach_ids.is_empty()
        {
            let _ = super::webchat_attachments::link(&mut tx, &conv.id, mid, &attach_ids).await;
        }
        if silenced {
            // R6: a resent accept of the message that queued the open item (its first accept timed out at the edge,
            // which then delivered nothing): the same `queued` with the notice, so the customer is told once
            if b.accept_only
                && let Some(item) = open_item.as_ref()
                && handover::queued_by(&mut tx, item, &b.message_id).await?
            {
                tx.commit().await?;
                return Ok(Json(InboundResp::Queued {
                    conversation_id: conv.id.clone(),
                    case_id: item.case_id.clone(),
                    item_id: item.id.clone(),
                    notice: true,
                }));
            }
            tx.commit().await?;
            return Ok(duplicate);
        }
        let state = match stored {
            Some(id) => crate::turn::message_state(&st, &scope, &conv, id).await,
            None => MessageState::Unknown,
        };
        // R6: a resent accept of a message still waiting for its answer (the edge died after storing it) is accepted
        // again, so the edge that acks this resend runs the answer; an accept never answers
        // `Some(true)`: accepted again, `Some(false)`: a duplicate, `None`: answer it below
        let accepted = match state {
            MessageState::Stored => b.accept_only.then_some(true),
            MessageState::Unknown if followed => Some(false),
            MessageState::Unknown => b.accept_only.then_some(true),
            MessageState::Queued | MessageState::Taken(_) => b.accept_only.then_some(false),
        };
        match accepted {
            Some(true) => {
                tx.commit().await?;
                return Ok(Json(InboundResp::Accepted {
                    conversation_id: conv.id.clone(),
                    case_id: case_id.unwrap_or_default(),
                }));
            }
            Some(false) => {
                tx.commit().await?;
                return Ok(duplicate);
            }
            None => {}
        }
        // a stuck accept the sweeper is answering right now (crate::conversations::answer_sweeper): it delivers the
        // answer, this call sends nothing; otherwise this call holds the answer (the sweeper leaves it alone)
        if answer_sweeper::take(&mut tx, &conv.id, &b.message_id).await? {
            tx.commit().await?;
            return Ok(duplicate);
        }
        let rt = customer_runtime(&st, &mut tx, runtime.as_ref(), &scope, &conv).await?;
        let answered = answer_stored(
            &st,
            tx,
            &rt,
            &scope,
            &conv,
            &b.message_id,
            case_id.as_deref(),
            stored,
            state,
        )
        .await?;
        return Ok(match answered {
            Some(r) => Json(r),
            // answered already, or a later message of the same turn delivers its one reply
            None => duplicate,
        });
    }
    // SW-9: the files this message carries must be this conversation's own unsent customer uploads (checked before
    // anything is stored; linked below, in the message's own transaction where it is ours)
    if !attach_ids.is_empty() {
        super::webchat_attachments::require_customer_unlinked(&mut tx, &conv.id, &attach_ids)
            .await?;
    }
    let author = b.party.name.clone().unwrap_or_else(|| b.party.id.clone());
    // SW-6 CSAT capture, only while nothing newer is going on (no open item, no open case; a/b also not staff control).
    // In order: (a) the comment after the low-rating follow-up, (b) the customer's reply to an agent's resolution
    // (acknowledgement → the prompt, anything else dismisses and is answered as usual), (c) the rating itself.
    let csat_idle = open_item.is_none() && store::open_case(&mut tx, &conv.id).await?.is_none();
    if csat_idle && !silenced {
        // (a) the answer to the follow-up question is the comment; its text is never logged, audited or labelled. A
        // question or a request for a person is not a comment: it is answered as usual (the comment stays empty)
        if !text.contains('?')
            && !human::asks_for_person(&text)
            && let Some(case_id) = csat::awaiting_comment(&mut tx, &conv.id).await?
            && store::last_outbound(&mut tx, &conv.id)
                .await?
                .is_some_and(|(role, author)| role == "agent" && author == csat::PROMPT_AUTHOR)
        {
            store::append(
                &mut tx,
                store::NewMessage {
                    job_id: b.job_id,
                    conversation_id: &conv.id,
                    role: "customer",
                    author: &author,
                    text: &text,
                    case_id: Some(&case_id),
                    external_id: Some(&b.message_id),
                },
            )
            .await?;
            csat::record_comment(&mut tx, &case_id, &text).await?;
            audit::record(
                &mut tx,
                &Actor::system("service:agent-edge"),
                Event::new("case.csat_comment", "case", case_id.clone())
                    .job(&scope.job_key)
                    .detail("channel", kind),
            )
            .await?;
            tx.commit().await?;
            staff::publish(
                &st,
                &scope,
                &conv.id,
                "csat",
                json!({"conversation_id": conv.id, "case_id": case_id, "comment": true}),
            )
            .await;
            return Ok(Json(InboundResp::CsatRecorded {
                text: csat::COMMENT_THANKS.into(),
                conversation_id: Some(conv.id.clone()),
            }));
        }
        // (b) the agent resolved the case and armed a request: this message decides
        if let Some((case_id, expired)) = csat::awaiting(&mut tx, &conv.id).await? {
            if expired {
                if csat::dismiss(&mut tx, &case_id).await? {
                    dismissed.push(csat::Dismissal::SilenceExpired);
                }
            } else if csat::acknowledges(&text) {
                let prompt_key = format!("csat:{case_id}");
                store::append(
                    &mut tx,
                    store::NewMessage {
                        job_id: b.job_id,
                        conversation_id: &conv.id,
                        role: "customer",
                        author: &author,
                        text: &text,
                        case_id: Some(&case_id),
                        external_id: Some(&b.message_id),
                    },
                )
                .await?;
                csat::confirm(&mut tx, &case_id).await?;
                // the prompt is a customer-facing line: a lone 1–5 right after it is the rating
                store::append(
                    &mut tx,
                    store::NewMessage {
                        job_id: b.job_id,
                        conversation_id: &conv.id,
                        role: "agent",
                        author: csat::PROMPT_AUTHOR,
                        text: csat::PROMPT,
                        case_id: Some(&case_id),
                        external_id: Some(&prompt_key),
                    },
                )
                .await?;
                tx.commit().await?;
                staff::publish(
                    &st,
                    &scope,
                    &conv.id,
                    "csat",
                    json!({"conversation_id": conv.id, "case_id": case_id}),
                )
                .await;
                return Ok(Json(InboundResp::CsatRequest {
                    text: csat::PROMPT.into(),
                    conversation_id: Some(conv.id.clone()),
                }));
            } else if csat::dismiss(&mut tx, &case_id).await? {
                dismissed.push(csat::Dismissal::NewQuestion);
            }
        }
    }
    // (c) a lone rating right after the CSAT prompt (nothing newer: no open case, no item) is the rating; any other
    // message dismisses the request, and a request that ran out is dismissed by whatever comes next
    let rating = csat::lone_rating(&text);
    let mut pending = None;
    if csat_idle {
        if let Some(case_id) = csat::pending(&mut tx, &conv.id).await? {
            let prompt_last = store::last_outbound(&mut tx, &conv.id)
                .await?
                .is_some_and(|(role, author)| role == "agent" && author == csat::PROMPT_AUTHOR);
            if prompt_last {
                if rating.is_some() {
                    pending = Some(case_id);
                } else if csat::dismiss(&mut tx, &case_id).await? {
                    dismissed.push(csat::Dismissal::NonNumeric);
                }
            }
        } else if let Some(case_id) = csat::expired_asked(&mut tx, &conv.id).await?
            && csat::dismiss(&mut tx, &case_id).await?
        {
            dismissed.push(csat::Dismissal::SilenceExpired);
        }
    }
    if let (Some(rating), Some(case_id)) = (rating, pending) {
        store::append(
            &mut tx,
            store::NewMessage {
                job_id: b.job_id,
                conversation_id: &conv.id,
                role: "customer",
                author: &author,
                text: &text,
                case_id: Some(&case_id),
                external_id: Some(&b.message_id),
            },
        )
        .await?;
        let low = csat::record(&mut tx, &conv.id, &case_id, rating).await?;
        audit::record(
            &mut tx,
            &Actor::system("service:agent-edge"),
            Event::new("case.csat", "case", case_id.clone())
                .job(&scope.job_key)
                .detail("rating", rating)
                .detail("channel", kind),
        )
        .await?;
        if low {
            // 1 or 2: one follow-up, whose answer is the comment
            csat::ask_follow_up(&mut tx, &case_id).await?;
            let follow_up_key = format!("csat-follow-up:{case_id}");
            store::append(
                &mut tx,
                store::NewMessage {
                    job_id: b.job_id,
                    conversation_id: &conv.id,
                    role: "agent",
                    author: csat::PROMPT_AUTHOR,
                    text: csat::FOLLOW_UP,
                    case_id: Some(&case_id),
                    external_id: Some(&follow_up_key),
                },
            )
            .await?;
        }
        tx.commit().await?;
        staff::publish(
            &st,
            &scope,
            &conv.id,
            "csat",
            json!({"conversation_id": conv.id, "case_id": case_id, "rating": rating}),
        )
        .await;
        return Ok(Json(InboundResp::CsatRecorded {
            text: if low { csat::FOLLOW_UP } else { csat::THANKS }.into(),
            conversation_id: Some(conv.id.clone()),
        }));
    }
    // D109 (KH T6), B4 T9c (§0 R7): a message the agent would answer is a runtime turn — redacted, stored, queued
    // and drained by `turn::accept_customer_message`, answered once the runtime stored its reply (grounded in the
    // reply's transaction by the interaction daemon, §0 R3). 409 `job_not_live` without a live release, 503
    // `unavailable` while no runtime process serves the scope: never a generic model reply. R6's `accept_only`
    // stores it for the edge's second call. An open item, staff control and a request for a person keep the path
    // below.
    if !silenced && !human::asks_for_person(&text) {
        let rt = customer_runtime(&st, &mut tx, runtime.as_ref(), &scope, &conv).await?;
        tx.commit().await?;
        let m = crate::turn::Inbound {
            scope: &scope,
            conv: &conv,
            author: &author,
            actor: Actor::system("service:agent-edge"),
            text: &text,
            external_id: Some(&b.message_id),
        };
        if b.accept_only {
            let accepted = crate::turn::store_customer_message(&st, m).await?;
            super::webchat_attachments::link_stored(
                &st,
                b.tenant,
                b.job_id,
                &conv.id,
                accepted.message_id,
                &attach_ids,
            )
            .await?;
            // armed for the sweeper: an accept whose edge never asks for the answer (every provider retry timed out)
            // is answered on the runtime and delivered by agent-api itself (crate::conversations::answer_sweeper)
            let mut tx = store::begin_job(&st.db, b.tenant, b.job_id).await?;
            answer_sweeper::arm(&mut tx, accepted.message_id).await?;
            tx.commit().await?;
            return Ok(Json(InboundResp::Accepted {
                conversation_id: conv.id.clone(),
                case_id: String::new(),
            }));
        }
        if !attach_ids.is_empty() {
            // SW-9: store, link the files, then run the turn (the turn would store the message itself, too late to link)
            let accepted = crate::turn::store_customer_message(&st, m).await?;
            super::webchat_attachments::link_stored(
                &st,
                b.tenant,
                b.job_id,
                &conv.id,
                accepted.message_id,
                &attach_ids,
            )
            .await?;
            let outcome = rt.stored_turn(&scope, &conv, accepted.message_id).await?;
            return Ok(Json(match outcome {
                Some(o) => runtime_resp(&conv.id, o),
                None => InboundResp::Duplicate {
                    conversation_id: Some(conv.id.clone()),
                },
            }));
        }
        let outcome = rt.turn(m).await?;
        return Ok(Json(runtime_resp(&conv.id, outcome)));
    }
    let design = store::design_of(&mut tx, b.job_id, &scope.job_key, &conv).await?;
    let case_id = store::case_for(&mut tx, b.job_id, &conv, &text, &design).await?;
    let Some(msg) = store::append(
        &mut tx,
        store::NewMessage {
            job_id: b.job_id,
            conversation_id: &conv.id,
            role: "customer",
            author: &author,
            text: &text,
            case_id: Some(&case_id),
            external_id: Some(&b.message_id),
        },
    )
    .await?
    else {
        tx.commit().await?;
        return Ok(Json(InboundResp::Duplicate {
            conversation_id: Some(conv.id.clone()),
        }));
    };
    let mut frame = json!({
        "conversation_id": conv.id, "message_id": msg.message_id, "role": "customer", "author": author,
        "text": text, "case_id": case_id, "at_ms": msg.at_ms,
    });
    if !attach_ids.is_empty() {
        super::webchat_attachments::link(&mut tx, &conv.id, msg.message_id, &attach_ids).await?;
        let refs =
            super::webchat_attachments::refs_by_message(&mut tx, &conv.id, &[msg.message_id])
                .await?
                .remove(&msg.message_id)
                .unwrap_or_default();
        frame["attachments"] = json!(refs);
    }
    // staff in control, or an open item: a person answers
    if let Some(item) = open_item {
        if item.state == HoState::WaitingCustomer {
            handover::move_to(
                &mut tx,
                b.job_id,
                &item,
                HoState::Assigned,
                "agent",
                "customer replied",
            )
            .await?;
        }
        tx.commit().await?;
        staff::publish(&st, &scope, &conv.id, "message", frame).await;
        return Ok(Json(InboundResp::Queued {
            conversation_id: conv.id,
            case_id: item.case_id,
            item_id: item.id,
            notice: false,
        }));
    }
    if silenced {
        tx.commit().await?;
        staff::publish(&st, &scope, &conv.id, "message", frame).await;
        return Ok(Json(InboundResp::Queued {
            conversation_id: conv.id,
            case_id,
            item_id: String::new(),
            notice: false,
        }));
    }
    // the customer asks for a person (every other message the agent would answer took the runtime branch above)
    let subject: String = text.chars().take(120).collect();
    let item_id = handover::create(
        &mut tx,
        handover::NewItem {
            job_id: b.job_id,
            case_id: &case_id,
            conv: &conv,
            reason: "customer_asked",
            trigger: "the customer asked for a person",
            subject: &subject,
            actor: "agent",
            design: &design,
        },
    )
    .await?;
    audit::record(
        &mut tx,
        &Actor::system("service:agent-edge"),
        Event::new("case.handover", "case", case_id.clone())
            .job(&scope.job_key)
            .detail("item_id", item_id.clone())
            .detail("reason", "customer_asked")
            .detail("channel", kind),
    )
    .await?;
    // the notice the edge sends for `notice: true`, in the transcript too (B13)
    handover::store_notice(&mut tx, b.job_id, &conv.id, &case_id).await?;
    let item = handover::view(&mut tx, &item_id).await?;
    tx.commit().await?;
    staff::publish(&st, &scope, &conv.id, "message", frame).await;
    staff::publish(&st, &scope, &conv.id, "handover", item).await;
    // the worker channels are told off the request (a database read and an edge round trip must not run on the
    // edge's accept budget); failures are logged by `notify_workers`
    {
        let (st, tenant, job_id, item_id) = (st.clone(), b.tenant, b.job_id, item_id.clone());
        tokio::spawn(async move {
            handover::notify_workers(&st, tenant, job_id, &item_id).await;
        });
    }
    Ok(Json(InboundResp::Queued {
        conversation_id: conv.id,
        case_id,
        item_id,
        notice: true,
    }))
}

/// R6: the answer to the stored customer message `external_id` of `conv` — a provider retry, the edge's answer call
/// for a message its accept stored, or the stuck-accept sweeper (crate::conversations::answer_sweeper) — on the
/// runtime (D109): where the runtime tracked the message (`state`, [`MessageState`]) its own turn decides (answered →
/// nothing, waiting → its reply, never queued → queued now); a runtime turn still running (T9c) is answered by the
/// runtime once. `tx` is the caller's [`store::begin_job`] transaction under the conversation's inbound lock; it is
/// committed before the runtime is awaited. `None`: nothing to send (answered already, or a later message of the same
/// turn delivers its one reply). Once the answer is settled the message's sweeper mark is cleared; a failed answer
/// keeps it (the sweeper tries again after its lease).
#[allow(clippy::too_many_arguments)]
pub(super) async fn answer_stored(
    st: &AppState,
    mut tx: sqlx::Transaction<'static, sqlx::Postgres>,
    rt: &RuntimeBackend,
    scope: &Scope,
    conv: &store::Conv,
    external_id: &str,
    case_id: Option<&str>,
    stored: Option<Uuid>,
    state: MessageState,
) -> ApiResult<Option<InboundResp>> {
    // T9c: a runtime turn still running (its case qualified, no reply yet) is answered by the runtime, once
    if state == MessageState::Unknown
        && let Some(case_id) = case_id
        && let Some((message_id, at_ms)) =
            runtime_backend::runtime_message(&mut tx, &conv.id, external_id, case_id).await?
    {
        tx.commit().await?;
        let outcome = rt.reply_after(scope, &conv.id, at_ms, message_id).await?;
        settled(st, scope, &conv.id, external_id).await?;
        return Ok(Some(runtime_resp(&conv.id, outcome)));
    }
    tx.commit().await?;
    let Some(message_id) = stored else {
        settled(st, scope, &conv.id, external_id).await?;
        return Ok(None);
    };
    let outcome = match state {
        // R6's stored message, or one whose turn never started: queued now (behind a running turn if any)
        MessageState::Stored | MessageState::Unknown => {
            rt.stored_turn(scope, conv, message_id).await?
        }
        // answered already: nothing; else the reply of the turn that took it
        MessageState::Taken(ms) => {
            if rt.after_take(scope, conv, ms, message_id).await?.is_some() {
                None
            } else {
                rt.taken_reply(scope, conv, message_id).await?
            }
        }
        MessageState::Queued => rt.taken_reply(scope, conv, message_id).await?,
    };
    settled(st, scope, &conv.id, external_id).await?;
    Ok(outcome.map(|o| runtime_resp(&conv.id, o)))
}

/// Clear the sweeper mark of the answered customer message `external_id` (crate::conversations::answer_sweeper).
async fn settled(
    st: &AppState,
    scope: &Scope,
    conversation_id: &str,
    external_id: &str,
) -> ApiResult<()> {
    let mut tx = store::begin_job(&st.db, scope.tenant, scope.job_id).await?;
    answer_sweeper::disarm(&mut tx, conversation_id, external_id).await?;
    tx.commit().await?;
    Ok(())
}

/// D109: the runtime that answers a customer message of `conv` (a [`store::begin_job`] transaction): 409
/// `job_not_live` while the job has no live release (a prod conversation; D130: a `test` one runs the draft design
/// in the test scope like /chat Draft, no release needed), 503 `unavailable` while no live `interaction` process
/// serves its scope. `runtime` was read for the conversation's own env (the binding's, or `conv.env` in the sweeper).
pub(super) async fn customer_runtime(
    st: &AppState,
    tx: &mut sqlx::PgConnection,
    runtime: Option<&RuntimeBackend>,
    scope: &Scope,
    conv: &store::Conv,
) -> ApiResult<RuntimeBackend> {
    if conv.env == runtime_backend::CUSTOMER_ENV {
        runtime_backend::job_live(st, tx, scope).await?;
    }
    runtime.cloned().ok_or_else(crate::turn::unavailable)
}

/// R6: the edge delivered item `item_id`'s handover notice. Scoped like `inbound` (the enabled, verified customer
/// binding of that tenant and job); stamped once, so an unknown or already stamped item is the same 204.
async fn notice_delivered(
    _k: EdgeKey,
    State(st): State<AppState>,
    ApiJson(b): ApiJson<NoticeDeliveredReq>,
) -> ApiResult<StatusCode> {
    store::require_clean([
        b.item_id.as_str(),
        b.conversation_id.as_str(),
        b.kind.as_str(),
    ])?;
    if [&b.item_id, &b.conversation_id]
        .iter()
        .any(|v| v.trim().is_empty() || v.chars().count() > 64)
    {
        return Err(ApiError::Unprocessable(
            "invalid_reference",
            "conversation_id and item_id are 1 to 64 characters".into(),
        ));
    }
    checked_binding(
        &st,
        b.binding_id,
        &b.kind,
        "customer",
        b.tenant,
        b.job_id,
        None,
    )
    .await?;
    let mut tx = store::begin_job(&st.db, b.tenant, b.job_id).await?;
    handover::mark_notice_delivered(&mut tx, &b.conversation_id, &b.item_id).await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// What the edge is told about a runtime turn: the reply to send; a newly queued handover (the edge sends the
/// notice once); staff in control (nothing to send).
fn runtime_resp(conversation_id: &str, o: Outcome) -> InboundResp {
    match o {
        Outcome::Replied { case_id, text, .. } => InboundResp::Answered {
            conversation_id: conversation_id.to_string(),
            case_id: case_id.unwrap_or_default(),
            text,
        },
        Outcome::Queued { case_id, item_id } => InboundResp::Queued {
            conversation_id: conversation_id.to_string(),
            case_id,
            item_id,
            notice: true,
        },
        Outcome::StaffControl => InboundResp::Queued {
            conversation_id: conversation_id.to_string(),
            case_id: String::new(),
            item_id: String::new(),
            notice: false,
        },
    }
}

fn not_staff() -> ApiError {
    ApiError::Forbidden(
        "not_staff",
        "Only an enabled member holding the Support agent role can answer from a worker channel."
            .into(),
    )
}

async fn staff_message(
    _k: EdgeKey,
    State(st): State<AppState>,
    ApiJson(b): ApiJson<StaffReq>,
) -> ApiResult<Json<StaffResp>> {
    store::require_clean(
        [
            b.text.as_str(),
            b.message_id.as_str(),
            b.staff.external_id.as_str(),
            b.staff.email.as_deref().unwrap_or_default(),
            b.staff.name.as_deref().unwrap_or_default(),
        ]
        .into_iter()
        .chain(ref_strings(&b.thread)),
    )?;
    let text = check_text(&b.text)?;
    check_ids(&b.thread, &b.message_id, "staff")?;
    let job_key = checked_binding(
        &st,
        b.binding_id,
        &b.kind,
        "worker",
        b.tenant,
        b.job_id,
        None,
    )
    .await?;
    let scope = Scope {
        tenant: b.tenant,
        job_id: b.job_id,
        job_key,
    };
    let (conversation_id, item_id) = {
        let mut tx = store::begin_job(&st.db, scope.tenant, scope.job_id).await?;
        let t = store::worker_thread(&mut tx, &b.kind, &b.thread.id).await?;
        tx.commit().await?;
        match t {
            Some((c, Some(i))) => (c, i),
            _ => {
                return Err(ApiError::Missing(
                    "thread_not_found",
                    "No handover item is linked to that thread.".into(),
                ));
            }
        }
    };
    let email = b
        .staff
        .email
        .as_deref()
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .ok_or_else(not_staff)?;
    let (sub, name) = {
        let mut tx = store::begin_job(&st.db, scope.tenant, scope.job_id).await?;
        let who = conversation_worker_by_email(&mut tx, email).await?;
        tx.commit().await?;
        who.ok_or_else(not_staff)?
    };
    let by = ownership::By {
        scope,
        actor: Actor::member(&sub, false),
        sub: sub.clone(),
    };
    // SW-5: the reply is a Take first, as in the workspace (the queue's or the agent's conversation becomes theirs;
    // theirs already changes nothing; 409 `owner_changed` while a colleague owns it, and the edge tells the thread;
    // 409 `closed` in a closed item's thread), then the one Send path
    ownership::take_scoped(&st, &by, &conversation_id, &item_id).await?;
    let out = staff::reply(
        &st,
        Reply {
            scope: by.scope,
            conversation_id,
            kind: MsgKind::Reply,
            actor: by.actor,
            sub,
            name,
            text,
            external_id: Some(b.message_id.clone()),
            client_msg_id: None,
            attachments: Vec::new(),
        },
    )
    .await?;
    Ok(Json(StaffResp {
        status: match out {
            staff::Outcome::Delivered => StaffStatus::Delivered,
            staff::Outcome::Stored => StaffStatus::Stored,
        },
        item_id,
    }))
}

/// The customer-facing line sent when a worker channel resolves the handover (stored as an `agent` line).
pub const RESOLVED_TEXT: &str = "Our support team has marked your request as resolved. If anything is still open, just reply here.";
/// The `author` of [`RESOLVED_TEXT`] in the transcript.
pub const RESOLVED_AUTHOR: &str = "resolution";

/// The enabled member holding `support_agent` or `support_manager` with `email` (case-insensitive): `(sub, display name)`.
async fn conversation_worker_by_email(
    conn: &mut sqlx::PgConnection,
    email: &str,
) -> ApiResult<Option<(String, String)>> {
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT sub, display_name FROM users \
          WHERE lower(email) = lower($1) AND disabled_at IS NULL AND {} \
          ORDER BY sub LIMIT 1",
        crate::auth::CONVERSATION_WORKERS_SQL
    )))
    .bind(email)
    .fetch_optional(conn)
    .await?;
    Ok(row.map(|r| {
        let sub: String = r.get("sub");
        let name = r
            .get::<Option<String>, _>("display_name")
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(|| sub.clone());
        (sub, name)
    }))
}

/// `POST /api/internal/channel/staff-state` (B7 R9): staff closed the handover of a worker thread in their own tool.
/// SW-5 (T5): the same commands as the workspace. The staff member takes the conversation
/// (crate::conversations::ownership::take_scoped: a queued item becomes theirs; 409 `owner_changed` while a colleague
/// owns it), then Resolves the thread's item with outcome `resolved` (crate::conversations::ownership::resolve_scoped):
/// the item and its case go to `resolved`, the agent owns the conversation again, CSAT is asked when enabled, and the
/// customer gets [`RESOLVED_TEXT`] (an `agent` line by [`RESOLVED_AUTHOR`]). An item already resolved is `duplicate`
/// (repeated webhooks change nothing, send nothing); one returned to the agent is 409 `closed`
/// (crate::conversations::handover::transition_error). The staff email must be an enabled member holding
/// `support_agent` (403 `not_staff`, logged).
async fn staff_state(
    _k: EdgeKey,
    State(st): State<AppState>,
    ApiJson(b): ApiJson<StaffStateReq>,
) -> ApiResult<Json<StaffStateResp>> {
    store::require_clean(
        [
            b.message_id.as_str(),
            b.state.as_str(),
            b.staff.external_id.as_str(),
            b.staff.email.as_deref().unwrap_or_default(),
            b.staff.name.as_deref().unwrap_or_default(),
        ]
        .into_iter()
        .chain(ref_strings(&b.thread)),
    )?;
    check_ids(&b.thread, &b.message_id, "staff")?;
    if b.state != HoState::Resolved.as_str() {
        return Err(ApiError::Unprocessable(
            "invalid_state",
            "state is resolved".into(),
        ));
    }
    let job_key = checked_binding(
        &st,
        b.binding_id,
        &b.kind,
        "worker",
        b.tenant,
        b.job_id,
        None,
    )
    .await?;
    let scope = Scope {
        tenant: b.tenant,
        job_id: b.job_id,
        job_key,
    };
    let (conversation_id, item_id) = {
        let mut tx = store::begin_job(&st.db, scope.tenant, scope.job_id).await?;
        let t = store::worker_thread(&mut tx, &b.kind, &b.thread.id).await?;
        tx.commit().await?;
        match t {
            Some((c, Some(i))) => (c, i),
            _ => {
                return Err(ApiError::Missing(
                    "thread_not_found",
                    "No handover item is linked to that thread.".into(),
                ));
            }
        }
    };
    let refused = || {
        tracing::info!(
            binding = %b.binding_id,
            kind = %b.kind,
            item = %item_id,
            "channel_staff_state_not_staff"
        );
        not_staff()
    };
    let email = b
        .staff
        .email
        .as_deref()
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .ok_or_else(refused)?;
    let (sub, state) = {
        let mut tx = store::begin_job(&st.db, scope.tenant, scope.job_id).await?;
        let who = conversation_worker_by_email(&mut tx, email).await?;
        let state = item_state(&mut tx, &item_id).await?;
        tx.commit().await?;
        (who.ok_or_else(refused)?.0, state)
    };
    let duplicate = Json(StaffStateResp {
        status: StaffStateStatus::Duplicate,
        item_id: item_id.clone(),
    });
    if state == HoState::Resolved {
        return Ok(duplicate);
    }
    // a queued item is taken below (the Take); anything else that cannot be resolved is refused here
    if let Some(code) =
        handover::transition_error(state, HoState::Resolved).filter(|c| *c != "assign_first")
    {
        return Err(ApiError::Conflict(
            code,
            format!("{item_id} cannot go from {} to resolved.", state.as_str()),
        ));
    }
    let by = ownership::By {
        scope,
        actor: Actor::member(&sub, false),
        sub: sub.clone(),
    };
    ownership::take_scoped(&st, &by, &conversation_id, &item_id).await?;
    let resolution = ownership::Resolution {
        outcome: "resolved".into(),
        closing_message: None,
        client_msg_id: None,
        feedback: None,
        // a worker-channel resolve: the server default (off after a complaint, on otherwise)
        ask_rating: None,
    };
    let worker = ownership::WorkerClose {
        channel: &b.kind,
        author: RESOLVED_AUTHOR,
        customer_text: RESOLVED_TEXT,
    };
    match ownership::resolve_scoped(&st, &by, &item_id, &sub, resolution, Some(worker)).await {
        Ok(_) => {}
        // resolved from the workspace in between: a repeated close
        Err(e) if e.code() == "closed" => {
            let mut tx = store::begin_job(&st.db, by.scope.tenant, by.scope.job_id).await?;
            let now = item_state(&mut tx, &item_id).await?;
            tx.commit().await?;
            if now == HoState::Resolved {
                return Ok(duplicate);
            }
            return Err(e);
        }
        Err(e) => return Err(e),
    }
    Ok(Json(StaffStateResp {
        status: StaffStateStatus::Resolved,
        item_id,
    }))
}

/// The state of handover item `item_id` (404 `case_not_found`).
async fn item_state(conn: &mut sqlx::PgConnection, item_id: &str) -> ApiResult<HoState> {
    let state: Option<String> =
        sqlx::query_scalar("SELECT state FROM agent_handover_items WHERE id = $1")
            .bind(item_id)
            .fetch_optional(conn)
            .await?;
    let state = state.ok_or_else(handover::not_found)?;
    HoState::parse(&state)
        .ok_or_else(|| ApiError::Internal(anyhow::anyhow!("unknown handover state")))
}

#[derive(Deserialize)]
struct MessagesQuery {
    #[serde(default)]
    after_ms: Option<i64>,
    tenant: Uuid,
    job_id: Uuid,
    binding_id: Uuid,
}

async fn conversation_messages(
    _k: EdgeKey,
    State(st): State<AppState>,
    ApiPath(conversation_id): ApiPath<String>,
    ApiQuery(q): ApiQuery<MessagesQuery>,
) -> ApiResult<Json<HistoryResp>> {
    let after = q.after_ms.unwrap_or(0).max(0);
    let kind: Option<String> =
        sqlx::query_scalar("SELECT kind FROM agent_connector_bindings() WHERE id = $1")
            .bind(q.binding_id)
            .fetch_optional(&st.db)
            .await?;
    let kind = kind.unwrap_or_default();
    checked_binding(
        &st,
        q.binding_id,
        &kind,
        "customer",
        q.tenant,
        q.job_id,
        None,
    )
    .await?;
    let not_found = || {
        ApiError::Missing(
            "conversation_not_found",
            "No such conversation on this binding.".into(),
        )
    };
    let mut tx = store::begin_job(&st.db, q.tenant, q.job_id).await?;
    // the conversation must have arrived on this binding
    let on_binding: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM channel_conversation_refs \
                         WHERE conversation_id = $1 AND side = 'customer' AND route->>'binding_id' = $2)",
    )
    .bind(&conversation_id)
    .bind(q.binding_id.to_string())
    .fetch_one(&mut *tx)
    .await?;
    if !on_binding {
        tx.commit().await?;
        return Err(not_found());
    }
    let msgs = store::messages_after(&mut tx, &conversation_id, after, 500).await?;
    let ids: Vec<Uuid> = msgs.iter().map(|m| m.message_id).collect();
    let mut files =
        super::webchat_attachments::refs_by_message(&mut tx, &conversation_id, &ids).await?;
    tx.commit().await?;
    Ok(Json(HistoryResp {
        messages: msgs
            .into_iter()
            .map(|m| HistoryMessage {
                attachments: files.remove(&m.message_id).unwrap_or_default(),
                role: m.role,
                text: m.text,
                at_ms: m.at_ms,
            })
            .collect(),
    }))
}

/// An edge time (epoch ms) as a timestamp, never later than `now` (an edge clock ahead of ours is clamped).
fn edge_time(
    ms: Option<i64>,
    now: chrono::DateTime<chrono::Utc>,
) -> ApiResult<Option<chrono::DateTime<chrono::Utc>>> {
    ms.map(|ms| {
        chrono::DateTime::from_timestamp_millis(ms)
            .filter(|t| t.timestamp() > 0)
            .map(|t| t.min(now))
            .ok_or_else(|| {
                ApiError::Unprocessable("invalid_time", "A time is out of range.".into())
            })
    })
    .transpose()
}

/// A report's webhook info with its error time clamped like every edge time ([`edge_time`]).
fn webhook_health_info(
    w: &WebhookInfoReport,
    now: chrono::DateTime<chrono::Utc>,
) -> ApiResult<WebhookInfoReport> {
    Ok(WebhookInfoReport {
        last_error_at_ms: edge_time(w.last_error_at_ms, now)?.map(|t| t.timestamp_millis()),
        ..w.clone()
    })
}

/// `POST /api/internal/channel/webhook-status` (B7 T15b): one binding's webhook health as the edge observed it. Only
/// a Telegram customer binding of the named tenant and job (any mismatch is 404 `binding_not_found`). The newest
/// update time, the newest successful send and the newest error are kept across edge restarts (the edge's counters
/// start empty) and across replicas; `degraded_at` follows crate::channels::webhook_health::degraded_since, so a
/// replica that saw no failures cannot clear another's `degraded` (0155). With the report's `webhook_info` (what
/// Telegram says, read by the edge) the status can also be `down` ([`webhook_health::down_reason`], stored in
/// `webhook_down_reason`), `pending_update_count` is stored, and Telegram's delivery error competes with the send
/// error under the same newer-wins rule; `webhook_info_at` records when, and a report without info keeps the stored
/// reason and count while that is younger than [`webhook_health::WEBHOOK_INFO_FRESH`] (replicas whose call fails do
/// not flap `down`). `url` is never written (the edge keeps it: it carries path ids).
async fn webhook_status(
    _k: EdgeKey,
    State(st): State<AppState>,
    ApiJson(b): ApiJson<WebhookStatusReq>,
) -> ApiResult<Json<WebhookStatusResp>> {
    if b.kind != "telegram" {
        return Err(ApiError::Unprocessable(
            "invalid_channel",
            "Webhook health is kept for Telegram bindings only.".into(),
        ));
    }
    if b.last_error.is_some() != b.last_error_at_ms.is_some()
        || b.last_error
            .as_deref()
            .is_some_and(|e| !webhook_health::is_error_code(e))
    {
        return Err(ApiError::Unprocessable(
            "invalid_error",
            "last_error is a short code (letters, digits, _ . : -) and comes with last_error_at_ms.".into(),
        ));
    }
    if let Some(w) = &b.webhook_info
        && (w.last_error_code.is_some() != w.last_error_at_ms.is_some()
            || w.last_error_code
                .as_deref()
                .is_some_and(|e| !webhook_health::is_error_code(e)))
    {
        return Err(ApiError::Unprocessable(
            "invalid_error",
            "webhook_info.last_error_code is a short code and comes with last_error_at_ms.".into(),
        ));
    }
    let now = chrono::Utc::now();
    let last_update = edge_time(b.last_update_at_ms, now)?;
    let last_send_ok = edge_time(b.last_send_ok_at_ms, now)?;
    let send_error_at = edge_time(b.last_error_at_ms, now)?;
    // Telegram's delivery error, its time clamped like every edge time
    let info = match &b.webhook_info {
        Some(w) => Some(webhook_health_info(w, now)?),
        None => None,
    };
    let webhook_error_at = info
        .as_ref()
        .and_then(|w| w.last_error_at_ms)
        .and_then(chrono::DateTime::from_timestamp_millis);
    // the newer of the send error and the webhook error is this report's error (newer-wins again on upsert)
    let (last_error, last_error_at) =
        match (b.last_error.as_deref(), send_error_at, webhook_error_at) {
            (_, s, Some(w)) if s.is_none_or(|s| w > s) => (
                info.as_ref().and_then(|i| i.last_error_code.as_deref()),
                Some(w),
            ),
            (e, s, _) => (e, s),
        };
    checked_binding(
        &st,
        b.binding_id,
        "telegram",
        "customer",
        b.tenant,
        b.job_id,
        None,
    )
    .await?;
    let mut tx = store::begin_job(&st.db, b.tenant, b.job_id).await?;
    type Ts = Option<chrono::DateTime<chrono::Utc>>;
    type Prev = (Ts, Ts, Ts, Ts, Option<String>, Option<i32>);
    let prev: Option<Prev> = sqlx::query_as(
        "SELECT last_update_at, last_send_ok_at, degraded_at, webhook_info_at, webhook_down_reason, \
                pending_update_count \
         FROM channel_webhook_status WHERE binding_id = $1 FOR UPDATE",
    )
    .bind(b.binding_id)
    .fetch_optional(&mut *tx)
    .await?;
    let (prev_update, prev_send_ok, prev_degraded, prev_info_at, prev_reason, prev_pending) =
        prev.unwrap_or_default();
    // the webhook info an earlier report left (stands in for a report without any while fresh, 0155)
    let stored_info = prev_info_at.map(|at| webhook_health::StoredInfo {
        at_ms: at.timestamp_millis(),
        down_reason: prev_reason
            .as_deref()
            .and_then(webhook_health::DownReason::parse),
        pending_update_count: prev_pending.and_then(|p| u32::try_from(p).ok()),
    });
    let ms = |t: Ts| t.map(|t| t.timestamp_millis());
    let newest_update = prev_update.max(last_update);
    let newest_send_ok = prev_send_ok.max(last_send_ok);
    let degraded_at_ms = webhook_health::degraded_since(
        ms(prev_degraded),
        ms(newest_send_ok),
        b.consecutive_send_failures,
        ms(send_error_at),
    );
    // a stored or a checked (clamped) edge time, so it converts back
    let degraded_at: Ts = degraded_at_ms.and_then(chrono::DateTime::from_timestamp_millis);
    let (status, down_reason) = webhook_health::health_of(
        now.timestamp_millis(),
        ms(newest_update),
        degraded_at_ms,
        ms(newest_send_ok),
        info.as_ref(),
        stored_info.as_ref(),
        webhook_health::STALE_AFTER,
    );
    let status = status.as_str();
    let down_reason = down_reason.map(webhook_health::DownReason::as_str);
    let info_at = info.is_some().then_some(now);
    // clamped to the column (integer); `None` without webhook info keeps the stored count
    let pending: Option<i32> = info
        .as_ref()
        .map(|w| i32::try_from(w.pending_update_count).unwrap_or(i32::MAX));
    sqlx::query(
        "INSERT INTO channel_webhook_status \
             (binding_id, tenant_id, job_id, status, last_update_at, last_error, last_error_at, last_send_ok_at, \
              degraded_at, pending_update_count, webhook_down_reason, webhook_info_at, observed_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, now()) \
         ON CONFLICT (binding_id) DO UPDATE SET \
             status = EXCLUDED.status, \
             webhook_down_reason = EXCLUDED.webhook_down_reason, \
             webhook_info_at = COALESCE(EXCLUDED.webhook_info_at, channel_webhook_status.webhook_info_at), \
             pending_update_count = COALESCE(EXCLUDED.pending_update_count, \
                 channel_webhook_status.pending_update_count), \
             last_update_at = GREATEST(channel_webhook_status.last_update_at, EXCLUDED.last_update_at), \
             last_send_ok_at = GREATEST(channel_webhook_status.last_send_ok_at, EXCLUDED.last_send_ok_at), \
             degraded_at = GREATEST(channel_webhook_status.degraded_at, EXCLUDED.degraded_at), \
             last_error = CASE WHEN EXCLUDED.last_error_at IS NOT NULL \
                     AND (channel_webhook_status.last_error_at IS NULL \
                          OR EXCLUDED.last_error_at >= channel_webhook_status.last_error_at) \
                 THEN EXCLUDED.last_error ELSE channel_webhook_status.last_error END, \
             last_error_at = GREATEST(channel_webhook_status.last_error_at, EXCLUDED.last_error_at), \
             observed_at = now()",
    )
    .bind(b.binding_id)
    .bind(b.tenant)
    .bind(b.job_id)
    .bind(status)
    .bind(last_update)
    .bind(last_error)
    .bind(last_error_at)
    .bind(last_send_ok)
    .bind(degraded_at)
    .bind(pending)
    .bind(down_reason)
    .bind(info_at)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(WebhookStatusResp {
        status: status.into(),
    }))
}

/// B7 known-customer web chat: the session's verified customer becomes the conversation's customer: conversation state
/// `subject`, `email`, `name` and `identity_source = assertion` (A1), and the conversation's `customer_id` (the wire
/// party column) follows the subject. Idempotent per message. A conversation that names a different customer is left
/// alone (logged by conversation id only). Customer data is not logged.
async fn bind_known_customer(
    conn: &mut sqlx::PgConnection,
    conversation_id: &str,
    placeholder: &str,
    kc: &super::webchat_sessions::KnownCustomer,
    staff: bool,
) -> ApiResult<()> {
    // staff: the party is the employee's lower-cased verified email (the assertion is signed by the binding's own
    // secret, so its email is verified), `crate::runtime::customer_context::staff_party`; no usable email, no binding
    let party_ref = if staff {
        let Some(email) = crate::runtime::customer_context::staff_party(kc.email.as_deref(), true)
        else {
            tracing::warn!(
                conversation_id,
                "staff session refused: the assertion carries no usable email"
            );
            return Ok(());
        };
        email
    } else {
        kc.customer_ref.clone()
    };
    // free to take: still the anonymous session placeholder (`wc:<session>`, identity source none or the channel's), or
    // already this customer
    let Some(cur) = super::store::conversation(&mut *conn, conversation_id).await? else {
        return Ok(());
    };
    let cur_id = cur.customer_id.as_deref();
    let placeholder_free = cur_id == Some(placeholder)
        && matches!(
            cur.identity_source.as_deref(),
            None | Some(super::state::SOURCE_CHANNEL)
        );
    if cur_id != Some(party_ref.as_str()) && !placeholder_free {
        tracing::warn!(
            conversation_id,
            "known customer session refused: the conversation already has another customer"
        );
        return Ok(());
    }
    sqlx::query("UPDATE conversation SET customer_id = $2 WHERE conversation_id = $1")
        .bind(conversation_id)
        .bind(&party_ref)
        .execute(&mut *conn)
        .await?;
    // the verified identity as state (subject, email, name, identity_source = assertion)
    super::state::put_known_identity(
        &mut *conn,
        conversation_id,
        super::state::SOURCE_ASSERTION,
        &party_ref,
        // staff: the same lower-cased email as the party id; customer: as given
        if staff {
            Some(party_ref.as_str())
        } else {
            kc.email.as_deref()
        },
        kc.name.as_deref(),
        "channel:web_chat",
    )
    .await?;
    Ok(())
}
