//! The drain (B4 T9): the lock holder takes the conversation's inbox and runs the turn, until the inbox stays empty.
//!
//! One turn, in order (step 0, card redaction, already ran before the message was stored):
//!
//! 1. **Licence gate** ([`LicenceGate`], R11; B7's [`crate::licensing::gate::VerdictGate`] composes B4's
//!    [`TenantStatusGate`] with the stored licence verdict): a tenant that is not active, or whose licence is not,
//!    gets the neutral [`super::REFUSAL_TEXT`] once per conversation per hour (`refused: true`) and no qualifier call; the
//!    audit row `channel_turn_refused` is written once per tenant per hour.
//! 2. **Not agent-owned** (SW-1, the one silence rule: the queue or a staff member owns the conversation): no agent
//!    turn; an open handover item absorbs the messages (they join its case; `waiting_customer` moves to `assigned`).
//! 3. (Merged into 2.)
//! 4. Otherwise: the scope's design (`Live` for prod, `Draft` for test) → the `RequestGate` hook (P1, B5; a no-op
//!    until then, crate::runtime::gates) → qualify through the job's `qualify` binding → one `usage_event` per model
//!    call → the case row, stamped with the design it was read at, and its `message` + `qualified` events in one
//!    transaction (R6: before the publish) → XADD `request.qualified` on `bus:customer:{area}`. A P1 `handover`
//!    verdict queues the case instead (reason `gate_blocked`, or `gate_unavailable` when the gate service did not
//!    answer, D94) and step 2 absorbs every later message of the conversation into that item (N-12); a `block`
//!    verdict answers [`GATE_BLOCKED_TEXT`] and runs nothing. The route hint of the verdict (F-19) travels on the
//!    `qualified` event and `request.qualified`.
//!
//! A turn that fails tells the conversation ([`super::TURN_FAILED`]); its messages are not re-queued, so an outage
//! cannot loop. Only ids and codes are logged.
use std::pin::Pin;

use serde_json::{Value, json};
use uuid::Uuid;

use super::qualify::{self, Draft, Outcome};
use super::redact;
use super::{
    Finish, HOUR_SECS, INFERENCE_NOT_PROVISIONED_TEXT, QUALIFY_UNBOUND_CODE, QUALIFY_UNBOUND_TEXT,
    REFUSAL_TEXT, TTL_SECS, TURN_FAILED,
};
use crate::audit::{self, Actor, Event};
use crate::auth::TenantStatus;
use crate::conversations::handover::{self, State as HoState};
use crate::conversations::{ownership, state, store};
use crate::design::JobRef;
use crate::error::{ApiError, ApiResult};
use crate::handover::hours::out_of_hours_text;
use crate::licensing::gate::VerdictGate;
use crate::providers::llm::{self, Use};
use crate::runtime::design::{RuntimeDesign, design_of};
use crate::runtime::domain::{QualifiedRequest, REQUEST_QUALIFIED};
use crate::runtime::gates::{Decision, GateVerdict, Gates, RequestInput, RouteHint, VerdictCause};
use crate::runtime::prompt::{self, SystemMessage};
use crate::runtime::resolve;
use crate::runtime::runner::agent_cases;
use crate::runtime::runner::declared::{INPUT_FIELD, InputPause, TaskAsk};
use crate::runtime::{Bus, Env, Envelope, Scope};
use crate::state::AppState;

/// The turn's audit actor (refusals; the runtime's own writes).
const SERVICE: &str = "service:agent-api-turn";

/// The reply of a turn the request gate blocked (P1 `block`): nothing else runs for it.
pub const GATE_BLOCKED_TEXT: &str =
    "Sorry, I can't help with that here. Please rephrase your question or ask for a person.";

/// Gate 2 at the drain (R11): may this tenant's turn run? B7's implementation is
/// [`crate::licensing::gate::VerdictGate`] (tenant status, then the licence verdict).
pub trait LicenceGate: Send + Sync {
    fn admits<'a>(
        &'a self,
        st: &'a AppState,
        tenant: Uuid,
    ) -> Pin<Box<dyn Future<Output = ApiResult<bool>> + Send + 'a>>;
}

/// B4's gate: only an `active` tenant's turns run (a suspended or archived tenant's get the neutral text).
pub struct TenantStatusGate;

impl LicenceGate for TenantStatusGate {
    fn admits<'a>(
        &'a self,
        st: &'a AppState,
        tenant: Uuid,
    ) -> Pin<Box<dyn Future<Output = ApiResult<bool>> + Send + 'a>> {
        Box::pin(async move {
            let status = crate::tenancy::tenant_status(&st.db, tenant).await?;
            Ok(status == Some(TenantStatus::Active))
        })
    }
}

/// Which conversation a drain serves.
#[derive(Clone, Debug)]
pub struct TurnCtx {
    pub tenant: Uuid,
    pub job_id: Uuid,
    pub job_key: String,
    pub env: Env,
    pub conversation_id: String,
}

impl TurnCtx {
    fn job(&self) -> JobRef {
        JobRef {
            id: self.job_id,
            key: self.job_key.clone(),
        }
    }
}

/// Drain the conversation while `token` holds its lock: one turn per batch of queued ids, until the inbox stays
/// empty (the lock is then released). Never fails: every error is logged (codes only) and told to the conversation.
/// Runs with the gate hooks on `st` (`AppState.gates`, B5's in-process hooks).
pub async fn run(st: AppState, ctx: TurnCtx, token: String) {
    let gates = st.gates.clone();
    run_with(st, ctx, token, gates).await
}

/// [`run`] with the gate hooks `gates` (B4's no-ops by default; B5's in-process hooks, test stubs).
pub async fn run_with(st: AppState, ctx: TurnCtx, mut token: String, gates: Gates) {
    let Some(root) = st.redis.clone() else {
        return;
    };
    let Ok(scope) = Scope::new(ctx.tenant, ctx.job_key.clone(), ctx.env) else {
        return;
    };
    let bus = root.scoped(&scope);
    loop {
        match super::take_inbox(&bus, &ctx.conversation_id).await {
            Ok(ids) if ids.is_empty() => {}
            Ok(ids) => {
                if let Err(e) = turn(&st, &bus, &ctx, &ids, &gates).await {
                    let (status, code) = e.status_and_code();
                    tracing::warn!(
                        status = status.as_u16(),
                        code,
                        conversation = %ctx.conversation_id,
                        "turn_failed"
                    );
                    if let Err(e) = reply(&st, &bus, &ctx, TURN_FAILED, false).await {
                        tracing::warn!(code = e.status_and_code().1, "turn_failed_notice_failed");
                    }
                }
            }
            Err(_) => return,
        }
        match super::finish(&bus, &ctx.conversation_id, &token).await {
            Ok(Finish::Pending) => continue,
            Ok(Finish::Released) | Err(_) => return,
            Ok(Finish::Lost) => {
                // the lock expired under a long turn: ids left behind need a holder, take it back when free
                tracing::warn!(conversation = %ctx.conversation_id, "turn_lock_expired");
                let next = Uuid::new_v4().to_string();
                match super::acquire(&bus, &ctx.conversation_id, &next).await {
                    Ok(true) => token = next,
                    _ => return,
                }
            }
        }
    }
}

/// One turn over the customer messages `ids` (see the module doc).
async fn turn(
    st: &AppState,
    bus: &Bus,
    ctx: &TurnCtx,
    ids: &[String],
    gates: &Gates,
) -> ApiResult<()> {
    let ids: Vec<Uuid> = ids.iter().filter_map(|s| Uuid::parse_str(s).ok()).collect();
    // 1. gate 2
    if !VerdictGate.admits(st, ctx.tenant).await? {
        return refuse(st, bus, ctx).await;
    }
    let mut tx = store::begin_job(&st.db, ctx.tenant, ctx.job_id).await?;
    let conv = store::conversation_or_404(&mut tx, &ctx.conversation_id).await?;
    let msgs: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT message_id, text FROM conversation_message \
          WHERE message_id = ANY($1) AND role IN ('customer', 'caller') ORDER BY at_ms, message_id",
    )
    .bind(&ids)
    .fetch_all(&mut *tx)
    .await?;
    if msgs.is_empty() {
        tx.commit().await?;
        return Ok(());
    }
    // 2-3. anyone but the agent owns the conversation (SW-1, the one silence rule): no agent turn; an open handover
    // item absorbs the messages (a person answers)
    if ownership::silenced(&ownership::read(&mut tx, &conv.id).await?.owner) {
        if let Some(item) = handover::open_for_conversation(&mut tx, &conv.id).await? {
            return absorb(st, ctx, tx, &conv.id, &item, &ids).await;
        }
        tx.commit().await?;
        return Ok(());
    }
    tx.commit().await?;
    // 4. design → RequestGate (P1) → qualify → usage → case → publish
    let job = ctx.job();
    let of = design_of(ctx.env);
    let (design, resolved, unprovisioned) = {
        let mut tx = store::begin_job(&st.db, ctx.tenant, ctx.job_id).await?;
        // prod: the Live design the customer gate already loaded (D109, `AppState.designs`); test: the draft as is
        let d = match ctx.env {
            Env::Prod => {
                let scope = Scope::new(ctx.tenant, ctx.job_key.clone(), ctx.env)?;
                st.designs
                    .get_or_load(&mut tx, &st.design, &scope, &job)
                    .await?
            }
            Env::Test => {
                std::sync::Arc::new(RuntimeDesign::load(&mut tx, &st.design, &job, &of).await?)
            }
        };
        // a tenant with no inference-router licence yet (fail closed, never the platform key) is not an error of the
        // turn: it is answered below
        let (r, unprovisioned) =
            match llm::resolve(&mut tx, st, ctx.tenant, &job, Use::Qualify, &of).await {
                Ok(r) => (r, false),
                Err(e) if e.code() == llm::INFERENCE_NOT_PROVISIONED_CODE => (None, true),
                Err(e) => return Err(e),
            };
        tx.commit().await?;
        (d, r, unprovisioned)
    };
    // A2: a parked `input_required` question: this message is its answer, stored as state and never shown to the
    // request gate or the qualifier
    if resume_input(st, bus, ctx, &conv, &design, &msgs).await? {
        return Ok(());
    }
    let text = msgs
        .iter()
        .map(|(_, t)| t.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let verdict = gates
        .request
        .check(
            st,
            RequestInput {
                tenant: ctx.tenant,
                job: &job,
                env: ctx.env,
                conversation_id: &conv.id,
                text: &text,
                design: &design,
            },
        )
        .await;
    if let Some(reason) = verdict.handover_reason() {
        return gate_handover(st, bus, ctx, &conv, &design, &verdict, reason, &msgs).await;
    }
    if verdict.decision == Decision::Block {
        return reply(st, bus, ctx, GATE_BLOCKED_TEXT, false).await;
    }
    let hint = gates.hinter.hint(&verdict, &design);
    let model = match resolved {
        // the person who started the conversation (a staff test chat), else agent's service account
        Some(r) => {
            qualify::model(st, ctx.tenant, r.acting_for(conv.initiator_sub.as_deref())).await?
        }
        None => None,
    };
    let Some(model) = model else {
        // no router licence: a test chat says so (the tenant's setup is still running); prod keeps the neutral text
        if unprovisioned && ctx.env == Env::Test {
            return reply_coded(
                st,
                bus,
                ctx,
                INFERENCE_NOT_PROVISIONED_TEXT,
                false,
                Some(llm::INFERENCE_NOT_PROVISIONED_CODE),
            )
            .await;
        }
        tracing::warn!(code = "qualify_unbound", job = %ctx.job_key, unprovisioned, "turn_not_qualified");
        // a staff test chat is told what is missing (the tester can connect a model); a customer channel keeps the
        // neutral failure text
        if ctx.env == Env::Test {
            return reply_coded(
                st,
                bus,
                ctx,
                QUALIFY_UNBOUND_TEXT,
                false,
                Some(QUALIFY_UNBOUND_CODE),
            )
            .await;
        }
        return reply(st, bus, ctx, TURN_FAILED, false).await;
    };
    let first = msgs[0].0.to_string();
    let draft = load_draft(bus, &conv.id).await;
    // D108 (KH T5): the one system message from this design's layers, for the conversation's channel
    let prompt = prompt::assemble(&design.as_ref().into(), Some(&conv.channel), None);
    let known = conv.known_customer().is_some();
    // a parked question about which order (plan 2.3): the answer ("the rug", "the first one") maps to one of the
    // candidates the orchestrator stored, on the server; the customer never types an id. An answer that points at
    // none of them is an ordinary turn (the question is gone).
    if known && let Some(choice) = take_choice(bus, &conv.id).await {
        let fresh = choice.until_ms >= chrono::Utc::now().timestamp_millis();
        if let Some(i) = fresh
            .then(|| resolve::choose(&choice.picks, &text))
            .flatten()
        {
            let needs: Vec<&str> = choice.needs.iter().map(String::as_str).collect();
            let mut slots = choice.slots.clone();
            slots.extend(choice.picks[i].slots(&needs));
            save_draft(bus, &conv.id, None).await?;
            let d = Draft {
                context_space: choice.context_space.clone(),
                slots,
                awaiting: None,
                message_id: Some(first),
            };
            return qualified(st, bus, ctx, &conv, &design, &prompt, d, &msgs, hint).await;
        }
    }
    let (extracted, usage) = qualify::extract(
        &model,
        &text,
        draft.as_ref(),
        &design.spaces,
        &design.settings,
        &prompt,
    )
    .await;
    {
        let mut tx = store::begin_job(&st.db, ctx.tenant, ctx.job_id).await?;
        crate::usage::emit(
            &mut tx,
            &st.cfg.metering,
            &crate::usage::UsageEvent {
                job_id: Some(ctx.job_id),
                source: source_of(&conv),
                use_: "qualify",
                activity: None,
                conversation_id: Some(&conv.id),
                case_id: None,
                actor: &actor_of(&conv),
                usage: &usage,
            },
        )
        .await?;
        tx.commit().await?;
    }
    let ex = extracted?;
    // the sole-space fallback keeps what the extraction found, even though the classifier named no space
    let slots = ex.slots.clone();
    let (outcome, keep) = qualify::step(draft, ex, &design.spaces, &first);
    save_draft(bus, &conv.id, keep.as_ref()).await?;
    match outcome {
        // A one-space job has no menu to offer: the message belongs to its only space, deterministically. The
        // qualifier model is a classifier and answered "please close the project" with the generic menu on one
        // live turn and routed it on another (2026-11-07) — with exactly one space there is nothing to choose.
        Outcome::Unclear => match qualify::sole_space(&design.spaces) {
            Some(space) => {
                let mut d = Draft {
                    context_space: space.key.clone(),
                    slots: Default::default(),
                    awaiting: None,
                    message_id: Some(first),
                };
                // the classifier recognised no space but still extracted slots: the only space keeps them
                qualify::fold(space, &mut d, slots);
                qualified(st, bus, ctx, &conv, &design, &prompt, d, &msgs, hint).await
            }
            None => reply(st, bus, ctx, &qualify::unclear(&design.spaces), false).await,
        },
        Outcome::Ask(slot) => {
            let space = keep.as_ref().map_or("", |d| d.context_space.as_str());
            reply(st, bus, ctx, &qualify::question(&slot, space), false).await
        }
        Outcome::Ready(d) => qualified(st, bus, ctx, &conv, &design, &prompt, d, &msgs, hint).await,
    }
}

/// The identity level a conversation whose customer is known starts at ([`KNOWN_CUSTOMER_IDENTITY`]).
pub use crate::conversations::store::KNOWN_CUSTOMER_IDENTITY;

/// The identity level a request of `conv` is qualified at: 2 when the conversation's customer is known (persisted
/// `subject` state + `identity_source` in `assertion` / `staff_pick` / `staff_test`, one rule for every channel),
/// else 0. Only the conversation's state counts: a channel's own verified claim (`WireParty.verified`) gives nothing.
pub fn identity_of(conv: &store::Conv) -> u8 {
    conv.identity_level()
}

/// `staff_test` for the staff test chat, else `turn`.
fn source_of(conv: &store::Conv) -> &'static str {
    if conv.channel == "staff" {
        "staff_test"
    } else {
        "turn"
    }
}

/// The usage actor: the staff member of a test chat, else `callers:<channel>`.
fn actor_of(conv: &store::Conv) -> String {
    match (&conv.initiator_sub, conv.channel.as_str()) {
        (Some(sub), "staff") if !sub.trim().is_empty() => sub.clone(),
        _ => format!("callers:{}", conv.channel),
    }
}

/// Step 2: the turn's messages `ids` join the open `item`'s case; an item `waiting_customer` moves to `assigned`.
/// Commits `tx`, then publishes the item's `handover` frame.
async fn absorb(
    st: &AppState,
    ctx: &TurnCtx,
    mut tx: sqlx::Transaction<'static, sqlx::Postgres>,
    conversation_id: &str,
    item: &handover::Item,
    ids: &[Uuid],
) -> ApiResult<()> {
    link_messages(&mut tx, ids, &item.case_id).await?;
    if item.state == HoState::WaitingCustomer {
        handover::move_to(
            &mut tx,
            ctx.job_id,
            item,
            HoState::Assigned,
            "agent",
            "customer replied",
        )
        .await?;
    }
    let v = handover::view(&mut tx, &item.id).await?;
    tx.commit().await?;
    if let Ok(scope) = Scope::new(ctx.tenant, ctx.job_key.clone(), ctx.env) {
        crate::sse_hub::publish(st, &scope, ctx.job_id, conversation_id, "handover", v).await;
    }
    Ok(())
}

async fn link_messages(
    conn: &mut sqlx::PgConnection,
    ids: &[Uuid],
    case_id: &str,
) -> ApiResult<()> {
    sqlx::query(
        "UPDATE conversation_message SET case_id = $2 WHERE message_id = ANY($1) AND case_id IS NULL",
    )
    .bind(ids)
    .bind(case_id)
    .execute(conn)
    .await?;
    Ok(())
}

/// A P1 handover verdict (reason `gate_blocked` | `gate_unavailable`, D94): the case of the turn's messages with
/// its `message` + `gate` events and one handover item (the drain's step 2 absorbs the conversation's later
/// messages into it, N-12), then the handover frame, the workers' notification and the handover notice.
#[allow(clippy::too_many_arguments)]
async fn gate_handover(
    st: &AppState,
    bus: &Bus,
    ctx: &TurnCtx,
    conv: &store::Conv,
    design: &RuntimeDesign,
    verdict: &GateVerdict,
    reason: &'static str,
    msgs: &[(Uuid, String)],
) -> ApiResult<()> {
    let trigger = match verdict.cause {
        VerdictCause::Fired => "a request gate fired",
        VerdictCause::Unavailable => "the gate service did not answer",
    };
    handover_turn(
        st,
        bus,
        ctx,
        conv,
        design,
        reason,
        trigger,
        Some(verdict.event_body("P1")),
        msgs,
    )
    .await
}

/// [`gate_handover`]'s body for any reason: the case of the turn's messages (with a `gate` event when `gate` is
/// given), one handover item, the frame, the workers' notification and the notice.
#[allow(clippy::too_many_arguments)]
async fn handover_turn(
    st: &AppState,
    bus: &Bus,
    ctx: &TurnCtx,
    conv: &store::Conv,
    design: &RuntimeDesign,
    reason: &'static str,
    trigger: &str,
    gate: Option<serde_json::Value>,
    msgs: &[(Uuid, String)],
) -> ApiResult<()> {
    let title = msgs.first().map(|(_, t)| t.as_str()).unwrap_or_default();
    let ids: Vec<Uuid> = msgs.iter().map(|(id, _)| *id).collect();
    let mut tx = store::begin_job(&st.db, ctx.tenant, ctx.job_id).await?;
    // N-12 (one open J21 item per conversation): the open-item check and the insert run under the conversation's
    // control lock, the lock the orchestrator's `queue_failed` takes before its own check + insert. The turn lock
    // (a Redis `SET NX` on the conversation's `:lock` key, crate::turn::acquire) only serialises drains, not the
    // orchestrator, and step 2's check ran in an earlier transaction, before the gate call.
    //
    // Lock order: this is the FIRST lock of the transaction (begin_job only sets GUCs; nothing has been read FOR
    // UPDATE or written yet). Every other `control_key` holder takes it before any row lock too (queue_failed,
    // runtime/interaction.rs, conversations/staff.rs console gate, Take / Assign / Hand back in
    // conversations/ownership.rs),
    // so all of them order control -> rows. The rows taken here after it (the open item FOR UPDATE, the job-wide
    // `agent_case_seq` row of agent_next_case_id, the new case/item rows) are never held by a transaction that then
    // waits for this conversation's control lock, so no wait cycle can close. Taking it after `create_case` would
    // instead hold the job-wide case counter while waiting on one conversation's lock. The owner row is locked right
    // after it (ownership::guard, SW T5), as the ownership commands do, before the case counter and the item.
    ownership::guard(&mut tx, &conv.id).await?;
    if let Some(item) = handover::open_for_conversation(&mut tx, &conv.id).await? {
        // a failed case (or another path) queued an item since step 2: it holds the conversation, the messages join it
        tracing::info!(item = %item.id, reason, "turn_gate_handover_absorbed");
        return absorb(st, ctx, tx, &conv.id, &item, &ids).await;
    }
    let case_id = store::create_case(&mut tx, ctx.job_id, conv, title, &design.case).await?;
    link_messages(&mut tx, &ids, &case_id).await?;
    let author = conv
        .customer_name
        .clone()
        .or_else(|| conv.customer_id.clone())
        .unwrap_or_default();
    for (id, text) in msgs {
        case_event(
            &mut tx,
            ctx.job_id,
            &case_id,
            "message",
            json!({"from": "customer", "author": author, "text": text, "msg_id": id.to_string()}),
        )
        .await?;
    }
    if let Some(body) = gate {
        case_event(&mut tx, ctx.job_id, &case_id, "gate", body).await?;
    }
    // B4 T16a (F-14): closed at the database's now() (policy / calendar SELECTs, no lock), the item's clocks start
    // at the next open time and the customer hears when
    let closed = agent_cases::closed_at_now(&mut tx, &ctx.job(), &design.case).await?;
    let item_id = handover::create_with(
        &mut tx,
        handover::NewItem {
            job_id: ctx.job_id,
            case_id: &case_id,
            conv,
            reason,
            trigger,
            subject: title,
            actor: "agent",
            design: &design.case,
        },
        closed.as_ref().map(|(_, o)| o),
    )
    .await?;
    let v = handover::view(&mut tx, &item_id).await?;
    tx.commit().await?;
    tracing::info!(case = %case_id, item = %item_id, reason, "turn_gate_handover");
    if let Ok(scope) = Scope::new(ctx.tenant, ctx.job_key.clone(), ctx.env) {
        crate::sse_hub::publish(st, &scope, ctx.job_id, &conv.id, "handover", v).await;
    }
    handover::notify_workers(st, ctx.tenant, ctx.job_id, &item_id).await;
    let notice = match &closed {
        Some((x, _)) => out_of_hours_text(x.out_of_hours, &x.next_open_local),
        None => crate::channels::connector::outbound_text(
            &crate::channels::connector::Outbound::HandoverNotice { eta_minutes: None },
        )
        .unwrap_or_default(),
    };
    reply(st, bus, ctx, &notice, false).await
}

/// The case of a complete request, its events, then `request.qualified` (R6: the row exists before the publish). The
/// `qualified` event carries the layer markers of the turn's `prompt` (D108: `layers`, prod and test alike).
#[allow(clippy::too_many_arguments)]
async fn qualified(
    st: &AppState,
    bus: &Bus,
    ctx: &TurnCtx,
    conv: &store::Conv,
    design: &RuntimeDesign,
    prompt: &SystemMessage,
    d: Draft,
    msgs: &[(Uuid, String)],
    hint: Option<RouteHint>,
) -> ApiResult<()> {
    let Some(space) = design
        .spaces
        .by_key(&d.context_space)
        .filter(|s| !s.area.is_empty())
    else {
        return reply(st, bus, ctx, &qualify::unclear(&design.spaces), false).await;
    };
    let title = msgs.first().map(|(_, t)| t.as_str()).unwrap_or_default();
    let ids: Vec<Uuid> = msgs.iter().map(|(id, _)| *id).collect();
    let (release_id, draft_rev) = store::stamp(&design.case);
    let request_type = space.space().to_owned();
    let identity_level = identity_of(conv);
    let mut tx = store::begin_job(&st.db, ctx.tenant, ctx.job_id).await?;
    // A4 `case_per_thread`: a later message of a thread whose latest case the assistant resolved continues that case
    let continued = if design.job_design.roles.case_per_thread() {
        match thread_case(&mut tx, &conv.id).await? {
            // only a case the orchestrator also holds as resolved can be reopened (its REOPEN_CASE refuses anything
            // else): otherwise the message would join a case nobody answers again, so it opens a new one
            Some(id)
                if crate::runtime::case::CaseStore::new(bus.clone())
                    .is_resolved(&id)
                    .await
                    .unwrap_or(false) =>
            {
                Some(id)
            }
            _ => None,
        }
    } else {
        None
    };
    let case_id = match &continued {
        Some(id) => {
            sqlx::query(
                "UPDATE agent_case SET status = 'open', outcome = NULL, escalation_reason = NULL, \
                        resolved_at = NULL, duration_ms = NULL WHERE case_id = $1",
            )
            .bind(id)
            .execute(&mut *tx)
            .await?;
            id.clone()
        }
        None => store::create_case(&mut tx, ctx.job_id, conv, title, &design.case).await?,
    };
    sqlx::query(
        "UPDATE agent_case SET request_type = $2, context_space = $3, qualified_release_id = $4, \
                qualified_draft_rev = $5, initiator_sub = $6, updated_at = now() \
          WHERE case_id = $1",
    )
    .bind(&case_id)
    .bind(&request_type)
    .bind(&space.key)
    .bind(&release_id)
    .bind(draft_rev)
    .bind(&conv.initiator_sub)
    .execute(&mut *tx)
    .await?;
    link_messages(&mut tx, &ids, &case_id).await?;
    let author = conv
        .customer_name
        .clone()
        .or_else(|| conv.customer_id.clone())
        .unwrap_or_default();
    for (id, text) in msgs {
        case_event(
            &mut tx,
            ctx.job_id,
            &case_id,
            "message",
            json!({"from": "customer", "author": author, "text": text, "msg_id": id.to_string()}),
        )
        .await?;
    }
    case_event(
        &mut tx,
        ctx.job_id,
        &case_id,
        "qualified",
        json!({
            "activity": "", "object": "", "modality": "request", "slots": d.slots,
            "context_space": space.key, "request_type": request_type,
            "release_id": release_id, "draft_rev": draft_rev, "route_hint": hint,
            "layers": prompt.layers(), "prompt_warning": prompt.warning,
            "continued": continued.is_some(),
        }),
    )
    .await?;
    tx.commit().await?;
    let req = QualifiedRequest {
        case_id: case_id.clone(),
        conversation_id: conv.id.clone(),
        area: space.area.clone(),
        context_space: space.key.clone(),
        request_type,
        // B5 T12 (R15): the activity the qualified space serves; the toolsets (N2) and the tool gate read it
        activity: Some(space.activity.clone()).filter(|a| !a.is_empty()),
        object: None,
        modality: None,
        slots: d.slots,
        customer_id: conv.customer_id.clone(),
        identity_level,
        message_id: d.message_id,
        initiator_sub: conv.initiator_sub.clone(),
        qualified_release_id: release_id,
        qualified_draft_rev: draft_rev,
        route_hint: hint,
        layers: prompt.layers(),
        prompt_warning: prompt.warning.clone(),
    };
    let published = match Envelope::new(
        &space.topic(REQUEST_QUALIFIED),
        "interaction",
        &case_id,
        &req,
    ) {
        Ok(env) => bus.publish(&bus.streams.customer(&space.area), &env).await,
        Err(e) => Err(e),
    };
    if published.is_err() {
        // the case cannot run: close it, tell the customer (only the code is logged)
        tracing::warn!(case = %case_id, code = "publish_failed", "turn_not_published");
        let mut tx = store::begin_job(&st.db, ctx.tenant, ctx.job_id).await?;
        store::set_case_status(&mut tx, &case_id, "failed", Some("publish_failed")).await?;
        tx.commit().await?;
        return reply(st, bus, ctx, TURN_FAILED, false).await;
    }
    Ok(())
}

/// A4 `case_per_thread`: the case a new message of conversation `conversation_id` continues: its newest case, when
/// the assistant (no procedure) resolved it. Any other status (a case waiting for the customer, handed over, failed)
/// is not continued: the orchestrator runs a new round only on a resolved case.
async fn thread_case(
    conn: &mut sqlx::PgConnection,
    conversation_id: &str,
) -> ApiResult<Option<String>> {
    Ok(sqlx::query_scalar(
        "SELECT case_id FROM (SELECT case_id, status, procedure_name FROM agent_case \
                               WHERE conversation_id = $1 \
                               ORDER BY started_at DESC, case_id DESC LIMIT 1) latest \
          WHERE status = 'resolved' AND procedure_name IS NULL",
    )
    .bind(conversation_id)
    .fetch_optional(conn)
    .await?)
}

/// Append one case event (`TimelineEvent` minus `kind`/`at` in `body`).
async fn case_event(
    conn: &mut sqlx::PgConnection,
    job_id: Uuid,
    case_id: &str,
    kind: &str,
    body: serde_json::Value,
) -> ApiResult<()> {
    sqlx::query(
        "INSERT INTO case_event (tenant_id, job_id, case_id, kind, body) \
         VALUES (app_current_tenant(), $1, $2, $3, $4)",
    )
    .bind(job_id)
    .bind(case_id)
    .bind(kind)
    .bind(body)
    .execute(conn)
    .await?;
    Ok(())
}

/// Store and publish one agent line of the drain (a question, the unclear prompt, a refusal, a failure notice).
pub(super) async fn reply(
    st: &AppState,
    bus: &Bus,
    ctx: &TurnCtx,
    text: &str,
    refused: bool,
) -> ApiResult<()> {
    reply_coded(st, bus, ctx, text, refused, None).await
}

/// [`reply`] whose live `message` event also carries a machine `code` (the stored line is the text alone).
pub(super) async fn reply_coded(
    st: &AppState,
    bus: &Bus,
    ctx: &TurnCtx,
    text: &str,
    refused: bool,
    code: Option<&str>,
) -> ApiResult<()> {
    let mut tx = store::begin_job(&st.db, ctx.tenant, ctx.job_id).await?;
    let stored = store::append(
        &mut tx,
        store::NewMessage {
            job_id: ctx.job_id,
            conversation_id: &ctx.conversation_id,
            role: "agent",
            author: "agent",
            text,
            case_id: None,
            external_id: None,
        },
    )
    .await?
    .ok_or_else(|| ApiError::Internal(anyhow::anyhow!("an agent line was not stored")))?;
    if refused {
        sqlx::query("UPDATE conversation_message SET refused = true WHERE message_id = $1")
            .bind(stored.message_id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    let mut data = json!({
        "conversation_id": ctx.conversation_id, "message_id": stored.message_id.to_string(), "role": "agent",
        "author": "agent", "text": text, "at_ms": stored.at_ms,
    });
    if refused {
        data["refused"] = json!(true);
    }
    if let Some(code) = code {
        data["code"] = json!(code);
    }
    super::publish_message(bus, &ctx.conversation_id, &data).await;
    Ok(())
}

/// A refused turn: the neutral text once per conversation per hour, the audit row once per tenant per hour.
async fn refuse(st: &AppState, bus: &Bus, ctx: &TurnCtx) -> ApiResult<()> {
    let tenant_key = bus
        .root()
        .root_key(&format!("turn:refused:{}", ctx.tenant.hyphenated()));
    if super::first_within(bus, &tenant_key, HOUR_SECS).await? {
        let mut tx = crate::tenancy::begin_scoped(&st.db, ctx.tenant).await?;
        crate::tenancy::lock_tenant_row(&mut tx, ctx.tenant).await?;
        let id = ctx.conversation_id.clone();
        let event = Event::new("channel_turn_refused", "conversation", id)
            .job(&ctx.job_key)
            .detail("code", "tenant_not_active");
        audit::record(&mut tx, &Actor::system(SERVICE), event).await?;
        tx.commit().await?;
    }
    let conv_key = super::chat_key(bus, &ctx.conversation_id, ":refused");
    if super::first_within(bus, &conv_key, HOUR_SECS).await? {
        reply(st, bus, ctx, REFUSAL_TEXT, true).await?;
    }
    Ok(())
}

/// The conversation's open draft (`None` when there is none or it does not parse).
async fn load_draft(bus: &Bus, conversation_id: &str) -> Option<Draft> {
    let raw: redis::RedisResult<Option<String>> = redis::cmd("HGET")
        .arg(super::chat_key(bus, conversation_id, ""))
        .arg("draft")
        .query_async(&mut bus.conn())
        .await;
    match raw {
        Ok(Some(s)) => serde_json::from_str::<Draft>(&s).ok(),
        Ok(None) => None,
        Err(e) => {
            tracing::warn!(kind = ?e.kind(), "turn_draft_unreadable");
            None
        }
    }
}

/// The conversation's parked choice (the orchestrator's question about which order), read and cleared: it is answered
/// by this turn or gone.
async fn take_choice(bus: &Bus, conversation_id: &str) -> Option<resolve::Choice> {
    let key = super::chat_key(bus, conversation_id, "");
    let raw: redis::RedisResult<Option<String>> = redis::cmd("HGET")
        .arg(&key)
        .arg("choice")
        .query_async(&mut bus.conn())
        .await;
    let raw = raw.ok().flatten()?;
    let _: redis::RedisResult<i64> = redis::cmd("HDEL")
        .arg(&key)
        .arg("choice")
        .query_async(&mut bus.conn())
        .await;
    serde_json::from_str(&raw).ok()
}

/// The conversation's parked `input_required` question without clearing it.
async fn peek_input(bus: &Bus, conversation_id: &str) -> Option<InputPause> {
    let key = super::chat_key(bus, conversation_id, "");
    let raw: redis::RedisResult<Option<String>> = redis::cmd("HGET")
        .arg(&key)
        .arg(INPUT_FIELD)
        .query_async(&mut bus.conn())
        .await;
    serde_json::from_str(&raw.ok().flatten()?).ok()
}

/// The conversation's parked `input_required` question, read and cleared: answered by this turn or gone.
async fn take_input(bus: &Bus, conversation_id: &str) -> Option<InputPause> {
    let pause = peek_input(bus, conversation_id).await?;
    let key = super::chat_key(bus, conversation_id, "");
    let _: redis::RedisResult<i64> = redis::cmd("HDEL")
        .arg(&key)
        .arg(INPUT_FIELD)
        .query_async(&mut bus.conn())
        .await;
    Some(pause)
}

/// Park `pause` again (an empty answer, asked once more).
async fn keep_input(bus: &Bus, conversation_id: &str, pause: &InputPause) -> ApiResult<()> {
    let key = super::chat_key(bus, conversation_id, "");
    let raw = serde_json::to_string(pause).map_err(anyhow::Error::from)?;
    redis::pipe()
        .cmd("HSET")
        .arg(&key)
        .arg(INPUT_FIELD)
        .arg(raw)
        .ignore()
        .cmd("EXPIRE")
        .arg(&key)
        .arg(TTL_SECS)
        .ignore()
        .query_async::<()>(&mut bus.conn())
        .await
        .map_err(super::redis_failed)
}

/// A2, at message acceptance (before the message is stored or echoed): when the conversation waits for a SECRET answer
/// (`one-time code`, password), seal `text` as that state key now and return the placeholder `[redacted]` in its
/// place — the stored message, the echo frame, the case events and the model never see the value, and the drain
/// only checks that the key is present. An empty text stores nothing (the drain asks again). Not a secret question:
/// `red` is returned unchanged. A failed store is logged (code only) and leaves the key absent.
pub(super) async fn seal_secret_answer(
    st: &AppState,
    bus: &Bus,
    tenant: Uuid,
    job_id: Uuid,
    conversation_id: &str,
    text: &str,
    red: redact::Redacted,
) -> redact::Redacted {
    let Some(pause) = peek_input(bus, conversation_id).await else {
        return red;
    };
    if !pause.secret || !pause.fresh(chrono::Utc::now().timestamp_millis()) {
        return red;
    }
    let answer = text.trim();
    if !answer.is_empty() {
        let stored = match st.provider_keyring.as_deref() {
            None => Err(ApiError::Missing("secret_store_unavailable", String::new())),
            Some(kr) => {
                async {
                    let mut tx = store::begin_job(&st.db, tenant, job_id).await?;
                    state::put_secret(
                        &mut tx,
                        kr,
                        conversation_id,
                        &pause.key,
                        answer,
                        "user",
                        None,
                    )
                    .await?;
                    tx.commit().await?;
                    ApiResult::Ok(())
                }
                .await
            }
        };
        if let Err(e) = stored {
            tracing::warn!(code = e.status_and_code().1, key = %pause.key, "input_secret_not_stored");
        }
    }
    redact::Redacted {
        text: "[redacted]".to_owned(),
        changed: red.changed,
    }
}

/// A2: resume a parked `input_required` question with this turn's messages. `Ok(false)`: nothing was parked (or it
/// went stale): an ordinary turn. `Ok(true)`: the turn is handled — the answer was stored as state and the parked
/// request re-run as a new case (no model call, [`qualified`]), or the question was asked again, or (retries used up)
/// the conversation was handed over as `tool_failed`. The value never reaches the qualifier, the gate or a log.
async fn resume_input(
    st: &AppState,
    bus: &Bus,
    ctx: &TurnCtx,
    conv: &store::Conv,
    design: &RuntimeDesign,
    msgs: &[(Uuid, String)],
) -> ApiResult<bool> {
    let Some(pause) = take_input(bus, &conv.id).await else {
        return Ok(false);
    };
    if !pause.fresh(chrono::Utc::now().timestamp_millis()) {
        return Ok(false);
    }
    if let Some(task) = pause.task.clone() {
        return resume_task(st, bus, ctx, conv, design, msgs, pause, task).await;
    }
    let answer = msgs
        .iter()
        .map(|(_, t)| t.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let answer = answer.trim();
    let mut tx = store::begin_job(&st.db, ctx.tenant, ctx.job_id).await?;
    let stored = if pause.secret {
        // sealed at acceptance (`seal_secret_answer`): only its presence is checked
        state::present(&mut tx, &conv.id, &[pause.key.as_str()])
            .await?
            .get(&pause.key)
            .copied()
            .unwrap_or(false)
    } else if answer.is_empty() {
        false
    } else {
        state::put_value(&mut tx, &conv.id, &pause.key, answer, "user", None)
            .await
            .is_ok()
    };
    tx.commit().await?;
    if !stored {
        return match pause.clone().retry() {
            Some(again) => {
                keep_input(bus, &conv.id, &again).await?;
                reply(st, bus, ctx, &again.prompt, false).await?;
                Ok(true)
            }
            None => {
                tracing::info!(conversation = %conv.id, key = %pause.key, "turn_input_unanswered");
                handover_turn(
                    st,
                    bus,
                    ctx,
                    conv,
                    design,
                    "tool_failed",
                    "a required answer was not given",
                    None,
                    msgs,
                )
                .await?;
                Ok(true)
            }
        };
    }
    let first = pause
        .message_id
        .clone()
        .unwrap_or_else(|| msgs[0].0.to_string());
    save_draft(bus, &conv.id, None).await?;
    let prompt = prompt::assemble(&design.into(), Some(&conv.channel), None);
    let d = Draft {
        context_space: pause.context_space.clone(),
        slots: pause.slots.clone(),
        awaiting: None,
        message_id: Some(first),
    };
    qualified(st, bus, ctx, conv, design, &prompt, d, msgs, None).await?;
    Ok(true)
}

/// B2: the reply to an MCP task's question (crate::runtime::task_relay). It fills the parked schema field; the next
/// field is asked, or, once every required field is given, the answer goes to the task as the job's service actor
/// (`mcp_client::store::answer`). Nothing is re-run and no model is called. An empty or invalid reply is asked again
/// until the retries are used up, then the conversation is handed over.
#[allow(clippy::too_many_arguments)]
async fn resume_task(
    st: &AppState,
    bus: &Bus,
    ctx: &TurnCtx,
    conv: &store::Conv,
    design: &RuntimeDesign,
    msgs: &[(Uuid, String)],
    pause: InputPause,
    task: TaskAsk,
) -> ApiResult<bool> {
    use crate::mcp_client::store::{AnswerInput, answer as answer_task};
    use crate::mcp_client::tasks::{StaffActor, input_questions};
    use crate::runtime::task_relay;
    let mut tx = store::begin_job(&st.db, ctx.tenant, ctx.job_id).await?;
    let detail: Option<(String, Option<Value>)> =
        sqlx::query_as("SELECT status, last_detail FROM mcp_tasks WHERE task_id = $1")
            .bind(&task.task_id)
            .fetch_optional(&mut *tx)
            .await?;
    // a secret field was sealed at acceptance (`seal_secret_answer`): open it once and forget it
    let given = if pause.secret {
        let v = match st.provider_keyring.as_deref() {
            Some(kr) => state::get(&mut tx, kr, &conv.id, &pause.key).await?,
            None => None,
        };
        state::forget(&mut tx, &conv.id, &pause.key).await?;
        v.unwrap_or_default()
    } else {
        msgs.iter()
            .map(|(_, t)| t.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    };
    tx.commit().await?;
    let question = detail
        .as_ref()
        .filter(|(status, _)| status == "input_required")
        .and_then(|(_, d)| d.as_ref())
        .map(input_questions)
        .unwrap_or_default()
        .into_iter()
        .find(|q| q.key == task.question);
    let Some(q) = question else {
        reply_coded(
            st,
            bus,
            ctx,
            task_relay::CLOSED_TEXT,
            false,
            Some("mcp_task_closed"),
        )
        .await?;
        return Ok(true);
    };
    let mut collected = task.collected.clone();
    match task_relay::accept_field(&q.requested_schema, &task.field, &given) {
        Ok(v) => {
            collected.insert(task.field.clone(), v);
        }
        Err(why) => {
            return match pause.clone().retry() {
                Some(mut again) => {
                    if !why.is_empty() {
                        again.prompt = format!("{why}\n{}", again.prompt);
                    }
                    keep_input(bus, &conv.id, &again).await?;
                    reply(st, bus, ctx, &again.prompt, false).await?;
                    Ok(true)
                }
                None => {
                    tracing::info!(conversation = %conv.id, task_id = %task.task_id, "turn_task_input_unanswered");
                    handover_turn(
                        st,
                        bus,
                        ctx,
                        conv,
                        design,
                        "tool_failed",
                        "a task's question was not answered",
                        None,
                        msgs,
                    )
                    .await?;
                    Ok(true)
                }
            };
        }
    }
    let now = chrono::Utc::now().timestamp_millis();
    if let Some(field) = task_relay::next_field(&q.requested_schema, &collected) {
        let next = task_relay::task_pause(
            &pause.case_id,
            &task.task_id,
            &q,
            &field,
            collected,
            false,
            now,
        );
        keep_input(bus, &conv.id, &next).await?;
        reply(st, bus, ctx, &next.prompt, false).await?;
        return Ok(true);
    }
    let input = AnswerInput {
        key: task.question.clone(),
        action: "accept".to_owned(),
        content: Some(serde_json::to_value(&collected).map_err(anyhow::Error::from)?),
    };
    let actor = StaffActor::for_job(ctx.tenant, ctx.job_id);
    match answer_task(
        &st.db,
        &st.task_clients(),
        &actor,
        ctx.job_id,
        &task.task_id,
        &input,
        // audit: the customer's messages behind the job's answer (fr records only the job)
        &msgs
            .iter()
            .map(|(id, _)| id.to_string())
            .collect::<Vec<_>>(),
    )
    .await
    {
        Ok(_) => {
            reply_coded(
                st,
                bus,
                ctx,
                task_relay::ANSWERED_TEXT,
                false,
                Some("mcp_task_answered"),
            )
            .await?;
        }
        Err(ApiError::Unavailable("mcp_unconfigured", _, _)) => {
            reply_coded(
                st,
                bus,
                ctx,
                task_relay::CLOSED_TEXT,
                false,
                Some("mcp_unavailable"),
            )
            .await?;
        }
        Err(ApiError::Unprocessable(_, why)) => {
            // the server's schema refused the whole answer: start the question over
            let Some(first) = task_relay::next_field(&q.requested_schema, &Default::default())
            else {
                return Ok(true);
            };
            let mut again = task_relay::task_pause(
                &pause.case_id,
                &task.task_id,
                &q,
                &first,
                Default::default(),
                true,
                now,
            );
            again.prompt = format!("{why}\n{}", again.prompt);
            keep_input(bus, &conv.id, &again).await?;
            reply(st, bus, ctx, &again.prompt, false).await?;
        }
        Err(ApiError::Conflict(code, _)) => {
            tracing::info!(task_id = %task.task_id, code, "turn_task_answer_not_taken");
            reply_coded(
                st,
                bus,
                ctx,
                task_relay::CLOSED_TEXT,
                false,
                Some("mcp_task_closed"),
            )
            .await?;
        }
        Err(e) => return Err(e),
    }
    Ok(true)
}

/// Keep `draft` (or clear it) for the next turn.
async fn save_draft(bus: &Bus, conversation_id: &str, draft: Option<&Draft>) -> ApiResult<()> {
    let key = super::chat_key(bus, conversation_id, "");
    let mut pipe = redis::pipe();
    match draft {
        Some(d) => {
            let raw = serde_json::to_string(d).map_err(anyhow::Error::from)?;
            pipe.cmd("HSET")
                .arg(&key)
                .arg("draft")
                .arg(raw)
                .ignore()
                .cmd("EXPIRE")
                .arg(&key)
                .arg(TTL_SECS)
                .ignore();
        }
        None => {
            pipe.cmd("HDEL").arg(&key).arg("draft").ignore();
        }
    }
    pipe.query_async::<()>(&mut bus.conn())
        .await
        .map_err(super::redis_failed)
}
