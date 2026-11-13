//! SW-11: a staff note's private comment on the helpdesk ticket of the handover item it was written under, as a durable
//! outbox (0206). A note is queued in its own transaction ([`enqueue`]) when the conversation has an open item and the
//! job a verified, enabled helpdesk worker binding; the note's request then tries it once ([`mirror_now`]) and one loop
//! per replica drains the rest ([`spawn`]), so a note survives Frappe being down and the pod restarting.
//!
//! Every attempt holds a claim: a lease on `mirror_due_at` and a fresh `mirror_claim` token, taken in one statement
//! and committed before the ticket is called (the sweeper's through `agent_claim_note_mirror`, as 0134's stuck-accept
//! claim). Every outcome is written only while the token is still the row's, so two attempts never post one note
//! twice. The comment goes only through the edge's `/internal/worker-note` (an HD Ticket Comment, internal to the
//! helpdesk), never `/internal/deliver`: a note never reaches the customer.
//!
//! Outcomes: posted → `sent`; no ticket for the item [`TICKET_WAIT`] after the note, or no binding → `skipped`; refused
//! (`NoteError::Rejected`) or [`MAX_ATTEMPTS`] used → `failed`, which the staff workspace shows on the note; anything
//! else is tried again after [`retry_after`]. An attempt whose outcome is unknown (`NoteError::Unknown`, a timeout after
//! the request was sent) marks the row, and every later attempt asks the edge to look the comment up before posting.
use std::time::Duration;

use sqlx::{PgConnection, Row};
use uuid::Uuid;

use super::edge_link::NoteError;
use super::{handover, store};
use crate::error::ApiResult;
use crate::interaction::ConversationRef;
use crate::state::AppState;

/// How often the sweeper runs.
pub const SWEEP_EVERY: Duration = Duration::from_secs(30);
/// How long a claim holds a note: one edge call ([`super::edge_link::NOTE_TIMEOUT`]) plus the reads and writes around it.
pub const LEASE: Duration = Duration::from_secs(120);
/// Waits after failed attempt 1, 2, …; [`MAX_ATTEMPTS`] is one more than its length.
pub const BACKOFF: [Duration; 7] = [
    Duration::from_secs(30),
    Duration::from_secs(120),
    Duration::from_secs(600),
    Duration::from_secs(3600),
    Duration::from_secs(3600),
    Duration::from_secs(3600),
    Duration::from_secs(3600),
];
/// Attempts per note before it is `failed` (`gave_up`).
pub const MAX_ATTEMPTS: i32 = 8;
/// How long a note written while its item's ticket is still being opened waits for the ticket before it is `skipped`.
pub const TICKET_WAIT: Duration = Duration::from_secs(15 * 60);
/// How often such a note looks for its ticket again (not counted as an attempt).
pub const TICKET_POLL: Duration = Duration::from_secs(30);
/// Rows one [`sweep_once`] claims at most (one claim call each).
pub const SWEEP_CAP: usize = 50;

const _: () = assert!(MAX_ATTEMPTS as usize == BACKOFF.len() + 1);
const _: () = assert!(LEASE.as_secs() > super::edge_link::NOTE_TIMEOUT.as_secs() + 30);

fn secs(d: Duration) -> f64 {
    d.as_secs_f64()
}

/// The wait after `attempts` failed attempts; `None` once [`MAX_ATTEMPTS`] are used.
pub fn retry_after(attempts: i32) -> Option<Duration> {
    usize::try_from(attempts)
        .ok()?
        .checked_sub(1)
        .and_then(|i| BACKOFF.get(i))
        .copied()
}

/// The comment a note becomes: its author's display name (else their sub) and its text.
pub fn comment_body(author: &str, author_sub: &str, text: &str) -> String {
    let author = match author.trim() {
        "" => author_sub,
        a => a,
    };
    format!("Note from {author}: {text}")
}

/// Queue note `message_id` of `conversation_id` (in the note's own transaction) when the conversation has an open
/// handover item and the job a verified, enabled helpdesk worker binding; otherwise the note is not mirrored
/// (`mirror_state` stays NULL).
pub async fn enqueue(
    conn: &mut PgConnection,
    job_id: Uuid,
    conversation_id: &str,
    message_id: Uuid,
) -> ApiResult<bool> {
    let Some(item) = handover::open_for_conversation(conn, conversation_id).await? else {
        return Ok(false);
    };
    let bound: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM connector_bindings \
          WHERE job_id = $1 AND kind = 'helpdesk' AND side = 'worker' AND enabled AND verified_at IS NOT NULL)",
    )
    .bind(job_id)
    .fetch_one(&mut *conn)
    .await?;
    if !bound {
        return Ok(false);
    }
    sqlx::query(
        "UPDATE conversation_message \
            SET mirror_state = 'pending', mirror_item = $2, mirror_due_at = now(), mirror_claim = NULL, \
                mirror_attempts = 0 \
          WHERE message_id = $1 AND role = 'note'",
    )
    .bind(message_id)
    .bind(&item.id)
    .execute(conn)
    .await?;
    Ok(true)
}

#[derive(sqlx::FromRow)]
struct Claimed {
    tenant_id: Uuid,
    job_id: Uuid,
    conversation_id: String,
    message_id: Uuid,
    claim: Uuid,
    attempts: i32,
}

/// The note's own attempt right after its commit, in the background: claims it when it is still due.
pub fn mirror_now(st: &AppState, tenant: Uuid, job_id: Uuid, message_id: Uuid) {
    let st = st.clone();
    tokio::spawn(async move {
        let claimed = claim_own(&st, tenant, job_id, message_id).await;
        match claimed {
            Ok(Some(row)) => attempt(&st, &row).await,
            Ok(None) => {}
            Err(e) => tracing::warn!(code = e.code(), "note_mirror_claim_failed"),
        }
    });
}

async fn claim_own(
    st: &AppState,
    tenant: Uuid,
    job_id: Uuid,
    message_id: Uuid,
) -> ApiResult<Option<Claimed>> {
    let mut tx = store::begin_job(&st.db, tenant, job_id).await?;
    let row: Option<Claimed> = sqlx::query_as(
        "UPDATE conversation_message \
            SET mirror_due_at = now() + make_interval(secs => $2), mirror_claim = gen_random_uuid(), \
                mirror_attempts = mirror_attempts + 1 \
          WHERE message_id = $1 AND mirror_state = 'pending' AND mirror_due_at <= now() \
          RETURNING tenant_id, job_id, conversation_id, message_id, mirror_claim AS claim, mirror_attempts AS attempts",
    )
    .bind(message_id)
    .bind(secs(LEASE))
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(row)
}

/// Claim one due note of any tenant (a [`LEASE`] from now), or `None` when nothing is due.
async fn claim_one(st: &AppState) -> ApiResult<Option<Claimed>> {
    let row: Option<Claimed> = sqlx::query_as(
        "SELECT tenant_id, job_id, conversation_id, message_id, claim, attempts \
           FROM agent_claim_note_mirror($1, $2)",
    )
    .bind(1_i32)
    .bind(i32::try_from(LEASE.as_secs()).unwrap_or(i32::MAX))
    .fetch_optional(&st.db)
    .await?;
    Ok(row)
}

/// Claim due notes of every tenant one at a time, up to [`SWEEP_CAP`], trying each before claiming the next; how many
/// were tried. A database error of the claim is returned; one note's failure is logged and the rest go on.
pub async fn sweep_once(st: &AppState) -> ApiResult<usize> {
    let mut tried = 0;
    for _ in 0..SWEEP_CAP {
        let Some(row) = claim_one(st).await? else {
            break;
        };
        attempt(st, &row).await;
        tried += 1;
    }
    Ok(tried)
}

/// One attempt of a claimed note; its failure is logged (the row keeps its lease and is claimed again after it).
async fn attempt(st: &AppState, row: &Claimed) {
    match mirror_one(st, row).await {
        Ok(outcome) => {
            metrics::counter!("agent_note_mirror_total", "outcome" => outcome).increment(1);
        }
        Err(e) => tracing::warn!(
            tenant = %row.tenant_id,
            conversation = %row.conversation_id,
            code = e.code(),
            attempts = row.attempts,
            "note_mirror_failed"
        ),
    }
}

/// Where a claimed note goes: its item's ticket and the binding that opened it, or why it goes nowhere.
enum Target {
    Ticket {
        ticket: String,
        binding: Uuid,
        body: String,
        check_first: bool,
    },
    /// No ticket yet, and the note is still within [`TICKET_WAIT`].
    Wait,
    Skip(&'static str),
    /// The claim is no longer this attempt's (another attempt finished it, or re-claimed it after the lease).
    Lost,
}

/// Try claimed note `row` once; the outcome's metric label.
async fn mirror_one(st: &AppState, row: &Claimed) -> ApiResult<&'static str> {
    let mut tx = store::begin_job(&st.db, row.tenant_id, row.job_id).await?;
    let target = target(&mut tx, row).await?;
    let (ticket, binding, body, check_first) = match target {
        Target::Lost => {
            tx.commit().await?;
            return Ok("lost");
        }
        Target::Wait => {
            // looking for the ticket is not an attempt
            sqlx::query(
                "UPDATE conversation_message \
                    SET mirror_due_at = now() + make_interval(secs => $3), mirror_claim = NULL, \
                        mirror_attempts = GREATEST(mirror_attempts - 1, 0) \
                  WHERE message_id = $1 AND mirror_claim = $2",
            )
            .bind(row.message_id)
            .bind(row.claim)
            .bind(secs(TICKET_POLL))
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            return Ok("waiting_for_ticket");
        }
        Target::Skip(why) => {
            finish(&mut tx, row, "skipped", Some(why), None).await?;
            tx.commit().await?;
            return Ok("skipped");
        }
        Target::Ticket {
            ticket,
            binding,
            body,
            check_first,
        } => (ticket, binding, body, check_first),
    };
    tx.commit().await?;
    let Some(edge) = st.edge.clone() else {
        // queued while the edge was configured: kept for a replica that has it
        let mut tx = store::begin_job(&st.db, row.tenant_id, row.job_id).await?;
        reschedule(&mut tx, row, false, "edge_not_configured").await?;
        tx.commit().await?;
        return Ok("retry");
    };
    let thread = ConversationRef {
        channel: "helpdesk".into(),
        id: ticket.clone(),
        route: Default::default(),
    };
    let sent = edge
        .worker_note(
            row.tenant_id,
            row.job_id,
            &thread,
            binding,
            &body,
            check_first,
        )
        .await;
    let mut tx = store::begin_job(&st.db, row.tenant_id, row.job_id).await?;
    let outcome = match sent {
        Ok(()) => {
            finish(&mut tx, row, "sent", None, Some(&ticket)).await?;
            "sent"
        }
        Err(NoteError::Rejected(code)) => {
            finish(&mut tx, row, "failed", Some(code), Some(&ticket)).await?;
            "failed"
        }
        Err(NoteError::Unknown) => reschedule(&mut tx, row, true, "delivery_unknown").await?,
        Err(NoteError::Retry(code)) => reschedule(&mut tx, row, false, code).await?,
    };
    tx.commit().await?;
    Ok(outcome)
}

/// The claimed note's target, read under its claim.
async fn target(conn: &mut PgConnection, row: &Claimed) -> ApiResult<Target> {
    let Some(note) = sqlx::query(
        "SELECT author, author_sub, text, at_ms, mirror_item, mirror_unknown FROM conversation_message \
          WHERE message_id = $1 AND mirror_claim = $2 AND mirror_state = 'pending'",
    )
    .bind(row.message_id)
    .bind(row.claim)
    .fetch_optional(&mut *conn)
    .await?
    else {
        return Ok(Target::Lost);
    };
    let item: String = note.get("mirror_item");
    let at_ms: i64 = note.get("at_ms");
    let Some(r) = sqlx::query(
        "SELECT external_id, route FROM channel_conversation_refs \
          WHERE conversation_id = $1 AND item_id = $2 AND side = 'worker' AND channel = 'helpdesk' \
          ORDER BY updated_at DESC LIMIT 1",
    )
    .bind(&row.conversation_id)
    .bind(&item)
    .fetch_optional(&mut *conn)
    .await?
    else {
        let waited = super::now_ms().saturating_sub(at_ms);
        let wait_ms = i64::try_from(TICKET_WAIT.as_millis()).unwrap_or(i64::MAX);
        return Ok(if waited < wait_ms {
            Target::Wait
        } else {
            Target::Skip("no_ticket")
        });
    };
    let ticket: String = r.get("external_id");
    let route: std::collections::BTreeMap<String, String> =
        serde_json::from_value(r.get::<serde_json::Value, _>("route")).unwrap_or_default();
    // the binding that opened the ticket; a ref stored before the route carried it: the job's helpdesk worker binding
    let binding = match route.get("binding_id").and_then(|b| Uuid::parse_str(b).ok()) {
        Some(b) => Some(b),
        None => {
            sqlx::query_scalar(
                "SELECT id FROM connector_bindings \
                  WHERE job_id = $1 AND kind = 'helpdesk' AND side = 'worker' AND enabled AND verified_at IS NOT NULL \
                  ORDER BY created_at LIMIT 1",
            )
            .bind(row.job_id)
            .fetch_optional(&mut *conn)
            .await?
        }
    };
    let Some(binding) = binding else {
        return Ok(Target::Skip("no_binding"));
    };
    let author: String = note.get::<Option<String>, _>("author").unwrap_or_default();
    let author_sub: String = note
        .get::<Option<String>, _>("author_sub")
        .unwrap_or_default();
    let text: String = note.get("text");
    Ok(Target::Ticket {
        ticket,
        binding,
        body: comment_body(&author, &author_sub, &text),
        check_first: note.get("mirror_unknown"),
    })
}

/// End the claimed note in `state` (`sent`, `skipped` or `failed`).
async fn finish(
    conn: &mut PgConnection,
    row: &Claimed,
    state: &'static str,
    error: Option<&str>,
    ticket: Option<&str>,
) -> ApiResult<()> {
    sqlx::query(
        "UPDATE conversation_message \
            SET mirror_state = $3, mirror_due_at = NULL, mirror_claim = NULL, mirror_error = $4, \
                mirror_ticket = COALESCE($5, mirror_ticket), \
                mirrored_at = CASE WHEN $3 = 'sent' THEN now() END \
          WHERE message_id = $1 AND mirror_claim = $2",
    )
    .bind(row.message_id)
    .bind(row.claim)
    .bind(state)
    .bind(error)
    .bind(ticket)
    .execute(conn)
    .await?;
    Ok(())
}

/// Try the claimed note again after [`retry_after`] (`unknown`: this attempt's outcome is unknown, so every later
/// attempt looks the comment up first), or end it `failed` (`gave_up`) once [`MAX_ATTEMPTS`] are used; the metric label.
async fn reschedule(
    conn: &mut PgConnection,
    row: &Claimed,
    unknown: bool,
    error: &'static str,
) -> ApiResult<&'static str> {
    let Some(wait) = retry_after(row.attempts) else {
        tracing::warn!(
            tenant = %row.tenant_id,
            conversation = %row.conversation_id,
            attempts = row.attempts,
            "note_mirror_given_up"
        );
        finish(conn, row, "failed", Some("gave_up"), None).await?;
        return Ok("failed");
    };
    sqlx::query(
        "UPDATE conversation_message \
            SET mirror_due_at = now() + make_interval(secs => $3), mirror_claim = NULL, \
                mirror_unknown = mirror_unknown OR $4, mirror_error = $5 \
          WHERE message_id = $1 AND mirror_claim = $2",
    )
    .bind(row.message_id)
    .bind(row.claim)
    .bind(secs(wait))
    .bind(unknown)
    .bind(error)
    .execute(conn)
    .await?;
    Ok("retry")
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
                Ok(n) => tracing::info!(tried = n, "note_mirrors_tried"),
                Err(e) => tracing::warn!(code = e.code(), "note_mirror_sweep_failed"),
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eight_attempts_with_growing_waits() {
        assert_eq!(retry_after(0), None);
        assert_eq!(retry_after(1), Some(Duration::from_secs(30)));
        assert_eq!(retry_after(2), Some(Duration::from_secs(120)));
        assert_eq!(retry_after(7), Some(Duration::from_secs(3600)));
        assert_eq!(retry_after(MAX_ATTEMPTS), None);
    }

    #[test]
    fn the_comment_names_the_author_or_their_sub() {
        assert_eq!(
            comment_body(" Ann ", "u-1", "check the order"),
            "Note from Ann: check the order"
        );
        assert_eq!(comment_body("  ", "u-1", "x"), "Note from u-1: x");
    }
}
