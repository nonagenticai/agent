//! The stuck-accept sweeper (B7 R6 follow-up). An `accept_only` message the agent would answer is stored and armed
//! (`conversation_message.answer_due_at`, 0134) [`STUCK_AFTER`] out. The edge's second call answers it and clears the
//! mark (`internal::answer_stored`). When that call never comes (every provider retry timed out at the edge, or the
//! edge died after the ack), this loop answers the message through the same path — on the runtime (D109, KH T6: 409
//! `job_not_live` / 503 `unavailable` are failures of that attempt, never a model reply) — and delivers the answer
//! (or the handover notice of a turn that queued the case for a person) through the edge's `/internal/deliver` (as a
//! staff reply is delivered). A delivered handover notice is stamped (`handover::mark_notice_delivered`, as the
//! edge's own ack does), so a resend of the queuing accept never tells the customer twice ([`deliver_answer`]).
//!
//! One loop per agent-api replica serves every tenant: [`sweep_once`] claims due rows across tenants ONE at a time
//! through `agent_claim_unanswered` (0134: `FOR UPDATE SKIP LOCKED` plus a [`LEASE`] lease and a fresh claim token,
//! committed before any answer), and answers each before it claims the next, so no claimed row waits out its lease
//! behind the others. Each claimed row is then re-checked under the conversation's inbound lock with the provider-
//! retry test (`store::retry_state`: nothing followed it, no open item, no staff control) and its claim token, and its
//! lease is renewed there: a second replica, or the edge's own answer call ([`take`]), never answers it twice. The
//! answer is bounded by [`ANSWER_DEADLINE`] and its delivery by [`DELIVERY_ATTEMPTS`] × [`DELIVERY_TIMEOUT`] plus
//! [`DELIVERY_BACKOFF`], all inside the [`LEASE`] (asserted at compile time). A row that needs no answer is disarmed;
//! one whose answer failed or timed out stays leased and is tried again after the lease, at most [`MAX_ATTEMPTS`]
//! times. A failed delivery is retried as a delivery only: the reply is already stored, it is never answered again.
use std::time::Duration;

use sqlx::{PgConnection, Row};
use uuid::Uuid;

use super::edge_link::DeliverKind;
use super::{handover, internal, ownership, staff, store};
use crate::auth::TenantStatus;
use crate::channels::core::InboundResp;
use crate::error::ApiResult;
use crate::interaction::Scope;
use crate::runtime::backend::RuntimeBackend;
use crate::state::AppState;

/// How long an accepted message waits for the edge's answer call before the sweeper may answer it.
pub const STUCK_AFTER: Duration = Duration::from_secs(60);
/// How often the sweeper runs.
pub const SWEEP_EVERY: Duration = Duration::from_secs(30);
/// How long a claim (the sweeper's, or the edge answer call's via [`take`]) holds a message. The sweeper renews it
/// at the re-check under the inbound lock, just before the answer.
pub const LEASE: Duration = Duration::from_secs(300);
/// Claims per message before the sweeper gives up on it (logged).
pub const MAX_ATTEMPTS: i32 = 3;
/// Rows claimed per `agent_claim_unanswered` call: one, so each row's lease starts just before its own answer.
pub const BATCH: i32 = 1;
/// Rows one [`sweep_once`] claims at most (one claim call each).
pub const SWEEP_CAP: usize = 50;
/// Bound on one answer (`internal::answer_stored`; inner bounds: the runtime's `REPLY_WAIT` 100 s, the provider
/// 120 s). It is a backstop above those, so it fires only when they failed; dropping the answer mid-turn may then
/// re-run tools on the next attempt (bounded by `MAX_ATTEMPTS`), the lesser harm next to a lease that can lapse
/// under a live answer. A timeout leaves the row leased for its next attempt.
pub const ANSWER_DEADLINE: Duration = Duration::from_secs(150);
/// Bound on one delivery attempt of a stored answer.
pub const DELIVERY_TIMEOUT: Duration = Duration::from_secs(30);
/// Waits between delivery attempts: after the first failure, after the second.
pub const DELIVERY_BACKOFF: [Duration; 2] = [Duration::from_secs(2), Duration::from_secs(5)];
/// Delivery attempts of one stored answer before the sweeper gives up on delivering it (logged).
pub const DELIVERY_ATTEMPTS: usize = DELIVERY_BACKOFF.len() + 1;
/// Slack in [`LEASE`] for the claim, the tenant and conversation reads and the inbound-lock wait before the renewal.
const LEASE_MARGIN: Duration = Duration::from_secs(30);

const fn backoff_total_secs() -> u64 {
    let mut total = 0;
    let mut i = 0;
    while i < DELIVERY_BACKOFF.len() {
        total += DELIVERY_BACKOFF[i].as_secs();
        i += 1;
    }
    total
}

// the lease outlives the whole answer + delivery of its row: no other replica re-claims a row being answered
const _: () = assert!(
    LEASE.as_secs()
        > ANSWER_DEADLINE.as_secs()
            + DELIVERY_ATTEMPTS as u64 * DELIVERY_TIMEOUT.as_secs()
            + backoff_total_secs()
            + LEASE_MARGIN.as_secs()
);

/// The wait before delivery attempt `failed + 1` after `failed` failed attempts (1-based); `None` once
/// [`DELIVERY_ATTEMPTS`] have failed.
pub fn backoff(failed: usize) -> Option<Duration> {
    failed
        .checked_sub(1)
        .and_then(|i| DELIVERY_BACKOFF.get(i))
        .copied()
}

fn secs(d: Duration) -> i32 {
    i32::try_from(d.as_secs()).unwrap_or(i32::MAX)
}

/// Arm the stored customer message `message_id` (an `accept_only` accept): answered by the sweeper unless the edge's
/// answer call clears it within [`STUCK_AFTER`].
pub async fn arm(conn: &mut PgConnection, message_id: Uuid) -> ApiResult<()> {
    sqlx::query(
        "UPDATE conversation_message \
            SET answer_due_at = now() + make_interval(secs => $2), answer_claim = NULL, answer_attempts = 0 \
          WHERE message_id = $1",
    )
    .bind(message_id)
    .bind(f64::from(secs(STUCK_AFTER)))
    .execute(conn)
    .await?;
    Ok(())
}

/// Clear the mark of the customer message `external_id` (answered, or nothing to answer).
pub async fn disarm(
    conn: &mut PgConnection,
    conversation_id: &str,
    external_id: &str,
) -> ApiResult<()> {
    sqlx::query(
        "UPDATE conversation_message SET answer_due_at = NULL, answer_claim = NULL \
          WHERE conversation_id = $1 AND external_id = $2 AND answer_due_at IS NOT NULL",
    )
    .bind(conversation_id)
    .bind(external_id)
    .execute(conn)
    .await?;
    Ok(())
}

/// The edge's answer call for the armed message `external_id` (run under the conversation's inbound lock): `true`
/// when another caller — the sweeper or an earlier edge call — holds a live claim on it (that caller answers and
/// delivers; this call sends nothing). Otherwise the call claims it for [`LEASE`] with a token of its own (the sweeper
/// and a concurrent resend of the same message skip it meanwhile; [`disarm`] clears it once answered, and a claim
/// whose caller died expires with the lease) and answers it; an unarmed message is untouched.
pub async fn take(
    conn: &mut PgConnection,
    conversation_id: &str,
    external_id: &str,
) -> ApiResult<bool> {
    let row = sqlx::query(
        "SELECT message_id, (answer_claim IS NOT NULL AND answer_due_at > now()) AS held \
           FROM conversation_message \
          WHERE conversation_id = $1 AND external_id = $2 AND answer_due_at IS NOT NULL \
            FOR UPDATE",
    )
    .bind(conversation_id)
    .bind(external_id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(row) = row else {
        return Ok(false);
    };
    if row.get::<bool, _>("held") {
        return Ok(true);
    }
    sqlx::query(
        "UPDATE conversation_message \
            SET answer_due_at = now() + make_interval(secs => $2), answer_claim = gen_random_uuid() \
          WHERE message_id = $1",
    )
    .bind(row.get::<Uuid, _>("message_id"))
    .bind(f64::from(secs(LEASE)))
    .execute(&mut *conn)
    .await?;
    Ok(false)
}

#[derive(sqlx::FromRow)]
struct Claimed {
    tenant_id: Uuid,
    job_id: Uuid,
    conversation_id: String,
    message_id: Uuid,
    external_id: Option<String>,
    claim: Uuid,
    attempts: i32,
}

/// Claim one due message of any tenant (a [`LEASE`] from now), or `None` when nothing is due.
async fn claim_one(st: &AppState) -> ApiResult<Option<Claimed>> {
    let row: Option<Claimed> = sqlx::query_as(
        "SELECT tenant_id, job_id, conversation_id, message_id, external_id, claim, attempts \
           FROM agent_claim_unanswered($1, $2)",
    )
    .bind(BATCH)
    .bind(secs(LEASE))
    .fetch_optional(&st.db)
    .await?;
    Ok(row)
}

/// Claim due messages of every tenant one at a time, up to [`SWEEP_CAP`], answering each that still needs it before
/// claiming the next; returns how many were answered. A database error of the claim is returned; one message's
/// failure is logged and the rest go on.
pub async fn sweep_once(st: &AppState) -> ApiResult<usize> {
    let mut answered = 0;
    for _ in 0..SWEEP_CAP {
        let Some(row) = claim_one(st).await? else {
            break;
        };
        match answer_one(st, &row).await {
            Ok(true) => answered += 1,
            Ok(false) => {}
            Err(e) => tracing::warn!(
                tenant = %row.tenant_id,
                conversation = %row.conversation_id,
                code = e.code(),
                attempts = row.attempts,
                "stuck_accept_answer_failed"
            ),
        }
    }
    Ok(answered)
}

/// Clear `row`'s mark when this sweep still holds its claim.
async fn release(conn: &mut PgConnection, row: &Claimed) -> ApiResult<()> {
    sqlx::query(
        "UPDATE conversation_message SET answer_due_at = NULL, answer_claim = NULL \
          WHERE message_id = $1 AND answer_claim = $2",
    )
    .bind(row.message_id)
    .bind(row.claim)
    .execute(conn)
    .await?;
    Ok(())
}

/// Answer and deliver one claimed message; `false` when it needed no answer (or another holder has it).
async fn answer_one(st: &AppState, row: &Claimed) -> ApiResult<bool> {
    let active = matches!(
        crate::tenancy::tenant_status(&st.db, row.tenant_id).await?,
        Some(TenantStatus::Active)
    );
    let mut tx = store::begin_job(&st.db, row.tenant_id, row.job_id).await?;
    let conv = store::conversation(&mut tx, &row.conversation_id).await?;
    let cref = store::customer_ref(&mut tx, &row.conversation_id).await?;
    let (Some(external_id), Some(conv), Some((cref, _)), true) =
        (row.external_id.clone(), conv, cref, active)
    else {
        release(&mut tx, row).await?;
        tx.commit().await?;
        return Ok(false);
    };
    if row.attempts > MAX_ATTEMPTS {
        tracing::warn!(
            tenant = %row.tenant_id,
            conversation = %row.conversation_id,
            attempts = row.attempts,
            "stuck_accept_given_up"
        );
        release(&mut tx, row).await?;
        tx.commit().await?;
        return Ok(false);
    }
    let job_key = store::job_key(&mut tx, row.job_id).await?;
    let scope = Scope {
        tenant: row.tenant_id,
        job_id: row.job_id,
        job_key,
    };
    // D109: the runtime answers (as in `internal::inbound`, read before the conversation's lock is taken)
    let runtime = RuntimeBackend::serving(st, &scope, &conv.env).await;
    // the inbound handler's own lock (`internal::inbound`): one writer of the conversation at a time
    store::lock(
        &mut tx,
        &format!(
            "{}:{}:{}:{}",
            row.tenant_id, row.job_id, conv.channel, cref.id
        ),
    )
    .await?;
    // still ours: renew the lease, so it runs from here (just before the answer), not from the claim
    let ours: Option<Uuid> = sqlx::query_scalar(
        "UPDATE conversation_message SET answer_due_at = now() + make_interval(secs => $3) \
          WHERE message_id = $1 AND answer_claim = $2 \
          RETURNING message_id",
    )
    .bind(row.message_id)
    .bind(row.claim)
    .bind(f64::from(secs(LEASE)))
    .fetch_optional(&mut *tx)
    .await?;
    if ours.is_none() {
        // the edge's answer call took it meanwhile, or another sweep re-claimed it after the lease
        tx.commit().await?;
        return Ok(false);
    }
    // the provider-retry test of `internal::inbound`: nothing followed it and the agent owns the conversation (SW-1;
    // a runtime accept stores the message without a case: the turn qualifies it)
    let silenced = ownership::silenced(&ownership::read(&mut tx, &conv.id).await?.owner);
    let case_id = match store::retry_state(&mut tx, &conv.id, &external_id).await? {
        Some((case_id, false)) if !silenced => case_id,
        _ => {
            release(&mut tx, row).await?;
            tx.commit().await?;
            return Ok(false);
        }
    };
    let state = crate::turn::message_state(st, &scope, &conv, row.message_id).await;
    // not live / not served: this attempt fails, the claim stays leased and the next one tries again
    let rt = internal::customer_runtime(st, &mut tx, runtime.as_ref(), &scope, &conv).await?;
    let answer = tokio::time::timeout(
        ANSWER_DEADLINE,
        internal::answer_stored(
            st,
            tx,
            &rt,
            &scope,
            &conv,
            &external_id,
            case_id.as_deref(),
            Some(row.message_id),
            state,
        ),
    )
    .await;
    let Ok(out) = answer else {
        // the row stays leased (our claim): the next claim after the lease tries again, within MAX_ATTEMPTS
        metrics::counter!("agent_stuck_accept_answer_timeouts_total").increment(1);
        tracing::warn!(
            tenant = %row.tenant_id,
            conversation = %row.conversation_id,
            attempts = row.attempts,
            deadline_s = ANSWER_DEADLINE.as_secs(),
            "stuck_accept_answer_timeout"
        );
        return Ok(false);
    };
    match out? {
        Some(resp) => deliver_answer(st, &scope, &conv.id, &resp).await?,
        // answered already, or a later message of the same turn delivers its one reply
        None => return Ok(false),
    }
    Ok(true)
}

/// Deliver the sweeper's answer `resp` of a stuck accept of `conversation_id` through the edge's `/internal/deliver`:
/// an `answered` reply as text, a newly `queued` handover as the handover notice; anything else sends nothing. A
/// handover notice the edge took is stamped delivered (`handover::mark_notice_delivered`, as the edge's own
/// `handover/notice-delivered` ack does on its inbound path), so a provider's resend of the queuing accept is
/// `duplicate` and the customer is told once (B7 R6). A notice that was not delivered stays unstamped. Only the stamp
/// can fail (a database error); a failed delivery is logged by [`deliver`].
pub async fn deliver_answer(
    st: &AppState,
    scope: &Scope,
    conversation_id: &str,
    resp: &InboundResp,
) -> ApiResult<()> {
    match resp {
        InboundResp::Answered { text, .. } => {
            deliver(st, scope, conversation_id, DeliverKind::Text, text).await;
        }
        InboundResp::Queued {
            notice: true,
            item_id,
            ..
        } if deliver(st, scope, conversation_id, DeliverKind::HandoverNotice, "").await => {
            let mut tx = store::begin_job(&st.db, scope.tenant, scope.job_id).await?;
            handover::mark_notice_delivered(&mut tx, conversation_id, item_id).await?;
            tx.commit().await?;
        }
        // an undelivered notice stays unstamped; any other reply sends nothing
        _ => {}
    }
    Ok(())
}

/// Deliver the stored answer `text` (or the handover notice, `kind`), retrying the delivery only (never the answer)
/// up to [`DELIVERY_ATTEMPTS`] times, each bounded by [`DELIVERY_TIMEOUT`], waiting [`backoff`] between them. Only a
/// refused send is retried; a timed-out one may still land, so it ends the attempts. Giving up is logged and counted.
/// `true` when a send succeeded (`staff::notify_customer`'s `true`); `false` when every attempt was refused or one
/// timed out.
async fn deliver(
    st: &AppState,
    scope: &Scope,
    conversation_id: &str,
    kind: DeliverKind,
    text: &str,
) -> bool {
    let mut failed = 0;
    loop {
        let sent = tokio::time::timeout(
            DELIVERY_TIMEOUT,
            staff::notify_customer(st, scope, conversation_id, kind, text),
        )
        .await;
        match sent {
            Ok(true) => return true,
            Ok(false) => {}
            // a timed-out send may still land: resending it could tell the customer twice, so it is not retried
            Err(_) => {
                metrics::counter!("agent_stuck_accept_delivery_given_up_total").increment(1);
                tracing::warn!(
                    tenant = %scope.tenant,
                    conversation = conversation_id,
                    timeout_s = DELIVERY_TIMEOUT.as_secs(),
                    "stuck_accept_delivery_timeout"
                );
                return false;
            }
        }
        failed += 1;
        let Some(wait) = backoff(failed) else {
            metrics::counter!("agent_stuck_accept_delivery_given_up_total").increment(1);
            tracing::warn!(
                tenant = %scope.tenant,
                conversation = conversation_id,
                attempts = failed,
                "stuck_accept_delivery_given_up"
            );
            return false;
        };
        tokio::time::sleep(wait).await;
    }
}

/// Start the sweeper: [`sweep_once`] every [`SWEEP_EVERY`].
pub fn spawn(st: AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(SWEEP_EVERY);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            match sweep_once(&st).await {
                Ok(0) => {}
                Ok(n) => tracing::info!(answered = n, "stuck_accepts_answered"),
                Err(e) => tracing::warn!(code = e.code(), "stuck_accept_sweep_failed"),
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delivery_is_tried_three_times_with_2s_then_5s_waits() {
        assert_eq!(backoff(0), None);
        assert_eq!(backoff(1), Some(Duration::from_secs(2)));
        assert_eq!(backoff(2), Some(Duration::from_secs(5)));
        assert_eq!(backoff(3), None);
        assert_eq!(DELIVERY_ATTEMPTS, 3);
        // every failed attempt but the last is followed by a wait
        let waits = (1..DELIVERY_ATTEMPTS).filter_map(backoff).count();
        assert_eq!(waits, DELIVERY_ATTEMPTS - 1);
    }

    #[test]
    fn one_row_per_claim_and_the_lease_covers_answer_and_delivery() {
        assert_eq!(BATCH, 1);
        assert_eq!(SWEEP_CAP, 50);
        let worst = ANSWER_DEADLINE
            + DELIVERY_TIMEOUT * u32::try_from(DELIVERY_ATTEMPTS).unwrap()
            + DELIVERY_BACKOFF.iter().sum::<Duration>();
        assert_eq!(worst, Duration::from_secs(247));
        assert!(LEASE > worst + LEASE_MARGIN, "{LEASE:?} vs {worst:?}");
    }
}
