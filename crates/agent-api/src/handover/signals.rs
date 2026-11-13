//! What the handover queue tells discovery (B8, D64 feedback loop): [`discovery_signals`] reads, for the scoped job,
//! the prod items queued since a point in time with their reason, SLA outcome and the person's verdict. Test-env
//! items are left out (a staff test chat is not a customer signal). Ids, codes and the proposal only: no customer or
//! staff text (the item's subject and messages are data, never a signal).
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use sqlx::{PgConnection, Row};

use super::sla::{self, ClockState};
use crate::error::ApiResult;

/// The most signals one read returns.
pub const SIGNALS_LIMIT: i64 = 500;

/// One handed-over prod item, as discovery reads it.
#[derive(Clone, Debug, Serialize)]
pub struct DiscoverySignal {
    pub item_id: String,
    pub case_id: String,
    pub reason: String,
    pub priority: String,
    pub state: String,
    pub queued_at: DateTime<Utc>,
    /// Either SLA clock breached (open) or missed (stopped late).
    pub sla_breached: bool,
    /// `agent_should_handle` | `right_to_hand_over`, when a person gave one.
    pub verdict: Option<String>,
    /// The proposal attached to an `agent_should_handle` verdict.
    pub proposal: Option<Value>,
    pub drafted: bool,
}

/// The scoped job's prod items queued at or after `since`, oldest first (at most [`SIGNALS_LIMIT`]).
pub async fn discovery_signals(
    c: &mut PgConnection,
    since: DateTime<Utc>,
) -> ApiResult<Vec<DiscoverySignal>> {
    let rows = sqlx::query(
        "SELECT h.id, h.case_id, h.reason, h.priority, h.state, h.queued_at, h.first_response_due, h.resolve_due, \
                h.first_response_at, h.closed_at, now() AS now, \
                f.verdict, f.proposal, f.drafted_at IS NOT NULL AS drafted \
           FROM agent_handover_items h \
           LEFT JOIN agent_handover_feedback f \
             ON f.tenant_id = h.tenant_id AND f.job_id = h.job_id AND f.item_id = h.id \
          WHERE h.env = 'prod' AND h.queued_at >= $1 \
          ORDER BY h.queued_at, h.id LIMIT $2",
    )
    .bind(since)
    .bind(SIGNALS_LIMIT)
    .fetch_all(&mut *c)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            let queued_at: DateTime<Utc> = r.get("queued_at");
            let first_response_at: Option<DateTime<Utc>> = r.get("first_response_at");
            let closed_at: Option<DateTime<Utc>> = r.get("closed_at");
            let now: DateTime<Utc> = r.get("now");
            let bad = |s: ClockState| matches!(s, ClockState::Breached | ClockState::Missed);
            let first = sla::clock(
                queued_at,
                r.get("first_response_due"),
                first_response_at.or(closed_at),
                now,
            );
            let resolve = sla::clock(queued_at, r.get("resolve_due"), closed_at, now);
            DiscoverySignal {
                item_id: r.get("id"),
                case_id: r.get("case_id"),
                reason: r.get("reason"),
                priority: r.get("priority"),
                state: r.get("state"),
                queued_at,
                sla_breached: bad(first) || bad(resolve),
                verdict: r.get("verdict"),
                proposal: r.get("proposal"),
                drafted: r.get::<Option<bool>, _>("drafted").unwrap_or(false),
            }
        })
        .collect())
}
