//! `GET /api/support/inbox?tab=&reason=&channel=&priority=` (SW T7, the web's `Inbox`): the caller's conversations
//! across every job of the tenant, `{rows: InboxRow[], counts: {mine, unassigned, open}}`.
//!
//! - `tab` (default `open`): `mine` = owner `user:<me>`; `unassigned` = owner `queue`; `open` = owner is not the
//!   agent; `resolved` = owner is the agent and one of its handover items closed in the last 7 days.
//! - `reason` matches a handover reason code or its plain-language label (the web's filter sends the label);
//!   `channel` the conversation's channel; `priority` the item's (`normal` without one). `counts` ignore the tab and
//!   the filters.
//! - Sort (the web's rule): the due time — `first_response_due` before the first response (`first_response_at`), else
//!   `resolve_due` — ascending, rows without one last; then `waiting_since` ascending.
//! - The staff test chat (channel `staff`) is never listed or counted; both envs are. An ended conversation (a demo
//!   reset ends the test ones) is in no mine/unassigned/open row or count.
//! - One query reads every row (owner, owner name, newest item, last message, last customer message: joins and
//!   laterals, never a per-row lookup) and one reads the counts; `unread` is one Redis `MGET` for all rows.
//!
//! `InboxRow`: the item fields (`item_id`, `state`, the SLA times) are the OPEN item's; `reason_label` and `priority`
//! are the newest item's (an open one first), so a resolved row still names why it was handed over.
//! `sla_starts_at` = the open item's `clock_starts_at ?? queued_at`, else the last customer message; `waiting_since` =
//! the open item's `queued_at`, else the last customer message (else the last message).
//!
//! `unread`: the last customer message is newer than the caller's last `presence: viewing` of the conversation
//! (crate::support::presence, Redis key `support:seen:{sub}:{job}:{conversation}` under the environment prefix, 7 days).
//! Without Redis (or when it does not answer) every row reads `unread: false`.
use std::time::Duration;

use axum::extract::State;
use axum::response::Json;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{PgConnection, Row};
use uuid::Uuid;

use super::names;
use crate::conversations::handover::REASONS;
use crate::conversations::ownership::{Owner, OwnerState, OwnerView};
use crate::conversations::store::MEMBER_NAME_SQL;
use crate::conversations::{iso, iso_ms};
use crate::error::{ApiError, ApiQuery, ApiResult};
use crate::state::AppState;
use crate::tenancy::Member;

/// The most rows one inbox read returns (the soonest due first).
pub const INBOX_LIMIT: i64 = 500;
/// The most one `MGET` of the seen marks waits on Redis.
const SEEN_TIMEOUT: Duration = Duration::from_secs(2);

/// One inbox tab.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    Mine,
    Unassigned,
    Open,
    Resolved,
}

impl Tab {
    /// The tab a query names (`open` when none); 400 `invalid_query` for anything else.
    pub fn parse(s: Option<&str>) -> ApiResult<Self> {
        match s.map(str::trim).filter(|s| !s.is_empty()) {
            None | Some("open") => Ok(Tab::Open),
            Some("mine") => Ok(Tab::Mine),
            Some("unassigned") => Ok(Tab::Unassigned),
            Some("resolved") => Ok(Tab::Resolved),
            Some(_) => Err(ApiError::BadRequest(
                "invalid_query",
                "tab is mine, unassigned, open or resolved.".into(),
            )),
        }
    }

    /// The row predicate (`$1` is the caller's sub).
    fn predicate(self) -> &'static str {
        match self {
            Tab::Mine => "v.ended_at IS NULL AND cc.mode = 'staff' AND cc.by_sub = $1",
            Tab::Unassigned => "v.ended_at IS NULL AND cc.mode = 'queue'",
            Tab::Open => "v.ended_at IS NULL AND COALESCE(cc.mode, 'agent') <> 'agent'",
            Tab::Resolved => {
                "COALESCE(cc.mode, 'agent') = 'agent' AND EXISTS (SELECT 1 FROM agent_handover_items x \
                  WHERE x.tenant_id = v.tenant_id AND x.job_id = v.job_id AND x.conversation_id = v.conversation_id \
                    AND x.closed_at > now() - interval '7 days')"
            }
        }
    }
}

/// `last_message` of an [`InboxRow`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct LastMessage {
    pub kind: String,
    /// At most 140 characters.
    pub text: String,
    pub at: String,
}

/// One row of the inbox (the web's `InboxRow`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct InboxRow {
    pub job: String,
    pub job_name: String,
    pub conversation_id: String,
    pub item_id: Option<String>,
    pub display_name: String,
    /// The customer is known and verified: conversation state `subject` with an `identity_source` of `assertion`, `staff_pick` or `staff_test`.
    pub verified: bool,
    pub channel: String,
    pub reason_label: Option<&'static str>,
    pub priority: String,
    pub last_message: LastMessage,
    pub owner: OwnerView,
    pub state: Option<String>,
    pub first_response_due: Option<String>,
    pub first_response_at: Option<String>,
    pub resolve_due: Option<String>,
    pub sla_starts_at: String,
    pub waiting_since: String,
    pub unread: bool,
    /// The last customer message (unix ms), for [`mark_unread`].
    #[serde(skip)]
    pub customer_at_ms: Option<i64>,
}

/// The inbox projection; a caller appends `WHERE …` (and its order).
fn select_sql() -> String {
    format!(
        "SELECT j.key AS job_key, j.label AS job_label, v.conversation_id, v.channel, {name_sql} AS customer_name, \
                {known_sql} AS verified, \
                p.display_name AS party_name, \
                COALESCE(cc.mode, 'agent') AS mode, cc.by_sub, COALESCE(cc.since, v.started_at) AS since, \
                CASE WHEN cc.mode = 'staff' THEN \
                     COALESCE((SELECT {MEMBER_NAME_SQL} FROM users WHERE users.sub = cc.by_sub), \
                              agent_actor_name(cc.by_sub), cc.by_sub) END AS owner_name, \
                h.id AS item_id, h.auto_stamped, h.reason, h.priority, h.state, h.is_open, h.queued_at, h.clock_starts_at, \
                h.first_response_due, h.first_response_at, h.resolve_due, \
                lm.role AS last_kind, lm.text AS last_text, lm.at_ms AS last_at_ms, lc.at_ms AS customer_at_ms, \
                (extract(epoch FROM v.started_at) * 1000)::bigint AS started_ms, \
                CASE WHEN h.is_open THEN \
                     CASE WHEN h.first_response_at IS NULL THEN h.first_response_due ELSE h.resolve_due END \
                END AS due, \
                CASE WHEN h.is_open THEN (extract(epoch FROM h.queued_at) * 1000)::bigint \
                     ELSE COALESCE(lc.at_ms, lm.at_ms, (extract(epoch FROM v.started_at) * 1000)::bigint) \
                END AS waiting_ms \
           FROM conversation v \
           JOIN jobs j ON j.tenant_id = v.tenant_id AND j.id = v.job_id \
           LEFT JOIN conversation_control cc \
             ON cc.tenant_id = v.tenant_id AND cc.job_id = v.job_id AND cc.conversation_id = v.conversation_id \
           LEFT JOIN party p ON p.tenant_id = v.tenant_id AND p.id = v.party_id \
           LEFT JOIN LATERAL ( \
                SELECT i.id, i.auto_assigned_at IS NOT NULL AS auto_stamped, i.reason, i.priority, i.state, i.queued_at, i.clock_starts_at, i.first_response_due, \
                       i.first_response_at, i.resolve_due, \
                       i.state NOT IN ('resolved', 'returned_to_agent') AS is_open \
                  FROM agent_handover_items i \
                 WHERE i.tenant_id = v.tenant_id AND i.job_id = v.job_id AND i.conversation_id = v.conversation_id \
                 ORDER BY i.state IN ('resolved', 'returned_to_agent'), i.queued_at DESC, i.id DESC \
                 LIMIT 1) h ON true \
           LEFT JOIN LATERAL ( \
                SELECT m.role, m.text, m.at_ms FROM conversation_message m \
                 WHERE m.tenant_id = v.tenant_id AND m.job_id = v.job_id AND m.conversation_id = v.conversation_id \
                   AND NOT (m.role = 'system' AND m.author = 'owner') \
                 ORDER BY m.at_ms DESC, m.message_id DESC \
                 LIMIT 1) lm ON true \
           LEFT JOIN LATERAL ( \
                SELECT m.at_ms FROM conversation_message m \
                 WHERE m.tenant_id = v.tenant_id AND m.job_id = v.job_id AND m.conversation_id = v.conversation_id \
                   AND m.role = 'customer' \
                 ORDER BY m.at_ms DESC \
                 LIMIT 1) lc ON true",
        name_sql = crate::conversations::state::sub("v", crate::conversations::state::NAME),
        known_sql = crate::conversations::state::known_sql("v")
    )
}

/// One row of [`select_sql`] as an [`InboxRow`] (`unread` false until [`mark_unread`]).
fn row_of(r: &sqlx::postgres::PgRow) -> InboxRow {
    let ts = |c: &str| r.get::<Option<DateTime<Utc>>, _>(c);
    let started: i64 = r.get::<Option<i64>, _>("started_ms").unwrap_or(0);
    let conversation_id: String = r.get("conversation_id");
    let channel: String = r.get("channel");
    let open = r.get::<Option<bool>, _>("is_open") == Some(true);
    let item_id = if open {
        r.get::<Option<String>, _>("item_id")
    } else {
        None
    };
    let mode: String = r.get("mode");
    let owner = match (mode.as_str(), r.get::<Option<String>, _>("by_sub")) {
        ("staff", Some(sub)) => Owner::User(sub),
        ("queue", _) => Owner::Queue,
        _ => Owner::Agent,
    };
    let owner = OwnerView::of(
        &OwnerState {
            owner,
            since: r.get("since"),
            name: r.get("owner_name"),
        },
        item_id.clone(),
    )
    .with_auto(open && r.get::<Option<bool>, _>("auto_stamped") == Some(true));
    let last_at_ms: Option<i64> = r.get("last_at_ms");
    let customer_at_ms: Option<i64> = r.get("customer_at_ms");
    let open_ts = |c: &str| if open { ts(c).map(iso) } else { None };
    let sla_starts_at = match (open, ts("clock_starts_at").or_else(|| ts("queued_at"))) {
        (true, Some(t)) => iso(t),
        _ => iso_ms(customer_at_ms.or(last_at_ms).unwrap_or(started)),
    };
    let reason: Option<String> = r.get("reason");
    let display_name = names::display_name(
        r.get::<Option<String>, _>("party_name").as_deref(),
        r.get::<Option<String>, _>("customer_name").as_deref(),
        &channel,
        &conversation_id,
    );
    InboxRow {
        job: r.get("job_key"),
        job_name: r.get("job_label"),
        item_id,
        display_name,
        verified: r.get("verified"),
        channel,
        reason_label: reason.as_deref().map(names::reason_label),
        priority: r
            .get::<Option<String>, _>("priority")
            .unwrap_or_else(|| "normal".to_string()),
        last_message: LastMessage {
            kind: r
                .get::<Option<String>, _>("last_kind")
                .unwrap_or_else(|| "system".to_string()),
            text: r
                .get::<Option<String>, _>("last_text")
                .unwrap_or_default()
                .chars()
                .take(140)
                .collect(),
            at: iso_ms(last_at_ms.unwrap_or(started)),
        },
        owner,
        state: if open { r.get("state") } else { None },
        first_response_due: open_ts("first_response_due"),
        first_response_at: open_ts("first_response_at"),
        resolve_due: open_ts("resolve_due"),
        sla_starts_at,
        waiting_since: iso_ms(r.get::<Option<i64>, _>("waiting_ms").unwrap_or(started)),
        unread: false,
        customer_at_ms,
        conversation_id,
    }
}

/// The filters of one inbox read.
#[derive(Clone, Debug, Default)]
pub struct Filters {
    pub channel: Option<String>,
    pub priority: Option<String>,
    /// The reason codes a `reason` filter names (a code or a label); empty matches nothing.
    pub reasons: Option<Vec<String>>,
    /// D125: the caller is a support_agent without support_manager: only jobs whose team is empty or holds them (and
    /// the conversations they themselves own, wherever).
    pub restricted: bool,
}

/// The codes of [`REASONS`] that `reason` names: the code itself or its label ([`names::reason_label`]).
pub fn reason_codes(reason: &str) -> Vec<String> {
    let r = reason.trim();
    REASONS
        .iter()
        .filter(|c| **c == r || names::reason_label(c) == r)
        .map(|c| (*c).to_string())
        .collect()
}

/// The rows of `tab` for caller `me` in the transaction's tenant (every job), sorted and capped at [`INBOX_LIMIT`].
pub async fn list(
    conn: &mut PgConnection,
    me: &str,
    tab: Tab,
    f: &Filters,
) -> ApiResult<Vec<InboxRow>> {
    let sql = format!(
        "{} WHERE {} AND v.channel <> 'staff' \
           AND ($2::text IS NULL OR v.channel = $2) \
           AND ($3::text IS NULL OR COALESCE(h.priority, 'normal') = $3) \
           AND ($4::text[] IS NULL OR h.reason = ANY($4)) \
           AND (NOT $5::boolean OR {team} OR (cc.mode = 'staff' AND cc.by_sub = $1)) \
         ORDER BY due ASC NULLS LAST, waiting_ms ASC, v.conversation_id \
         LIMIT {INBOX_LIMIT}",
        select_sql(),
        tab.predicate(),
        team = super::job_staff::team_open_sql("v.tenant_id", "v.job_id", "$1")
    );
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(me)
        .bind(f.channel.as_deref())
        .bind(f.priority.as_deref())
        .bind(f.reasons.clone())
        .bind(f.restricted)
        .fetch_all(conn)
        .await?;
    Ok(rows.iter().map(row_of).collect())
}

/// The inbox row of conversation `conversation_id` of job `job_id` (the stream's `inbox_row` frame); `None` when it
/// does not exist or the inbox does not list it (the staff test chat).
pub async fn one(
    conn: &mut PgConnection,
    job_id: Uuid,
    conversation_id: &str,
) -> ApiResult<Option<InboxRow>> {
    let sql = format!(
        "{} WHERE v.job_id = $1 AND v.conversation_id = $2 AND v.channel <> 'staff'",
        select_sql()
    );
    let row = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(job_id)
        .bind(conversation_id)
        .fetch_optional(conn)
        .await?;
    Ok(row.as_ref().map(row_of))
}

/// `{mine, unassigned, open}` for caller `me` over every job of the transaction's tenant (tab and filters ignored; the
/// staff test chat is not counted).
pub async fn counts(conn: &mut PgConnection, me: &str, restricted: bool) -> ApiResult<Value> {
    let sql = format!(
        "SELECT count(*) FILTER (WHERE cc.mode = 'staff' AND cc.by_sub = $1) AS mine_n, \
                count(*) FILTER (WHERE cc.mode = 'queue') AS unassigned_n, \
                count(*) FILTER (WHERE cc.mode <> 'agent') AS open_n \
           FROM conversation_control cc \
           JOIN conversation v \
             ON v.tenant_id = cc.tenant_id AND v.job_id = cc.job_id AND v.conversation_id = cc.conversation_id \
          WHERE v.channel <> 'staff' AND v.ended_at IS NULL AND (NOT $2::boolean OR {} OR (cc.mode = 'staff' AND cc.by_sub = $1))",
        super::job_staff::team_open_sql("v.tenant_id", "v.job_id", "$1")
    );
    let r = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(me)
        .bind(restricted)
        .fetch_one(conn)
        .await?;
    Ok(json!({
        "mine": r.get::<i64, _>("mine_n"),
        "unassigned": r.get::<i64, _>("unassigned_n"),
        "open": r.get::<i64, _>("open_n"),
    }))
}

/// Set `unread` on `rows` for viewer `sub`: one `MGET` of the seen marks. Without Redis, or when it does not answer,
/// every row stays `unread: false`.
pub async fn mark_unread(st: &AppState, sub: &str, rows: &mut [InboxRow]) {
    let Some(root) = st.redis.as_ref() else {
        return;
    };
    let at: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, r)| r.customer_at_ms.is_some())
        .map(|(i, _)| i)
        .collect();
    if at.is_empty() {
        return;
    }
    let keys: Vec<String> = at
        .iter()
        .map(|i| super::presence::seen_key(root, sub, &rows[*i].job, &rows[*i].conversation_id))
        .collect();
    let mut c = root.conn();
    let mut cmd = redis::cmd("MGET");
    cmd.arg(&keys);
    let seen = match tokio::time::timeout(SEEN_TIMEOUT, cmd.query_async::<Vec<Option<i64>>>(&mut c))
        .await
    {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            tracing::warn!(error = %format!("{e:#}"), "support_seen_unreadable");
            return;
        }
        Err(_) => {
            tracing::warn!("support_seen_timed_out");
            return;
        }
    };
    for (i, seen) in at.into_iter().zip(seen) {
        let row = &mut rows[i];
        row.unread = row.customer_at_ms.unwrap_or(0) > seen.unwrap_or(0);
    }
}

#[derive(Deserialize)]
pub struct InboxQuery {
    tab: Option<String>,
    reason: Option<String>,
    channel: Option<String>,
    priority: Option<String>,
}

/// A query value, `None` when absent or blank.
fn given(v: Option<String>) -> Option<String> {
    v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// `GET /api/support/inbox` (see the module doc).
pub async fn support_inbox(
    Member(t): Member,
    State(st): State<AppState>,
    ApiQuery(q): ApiQuery<InboxQuery>,
) -> ApiResult<Json<Value>> {
    super::require_worker(&t)?;
    let tab = Tab::parse(q.tab.as_deref())?;
    let filters = Filters {
        channel: given(q.channel),
        priority: given(q.priority),
        reasons: given(q.reason).map(|r| reason_codes(&r)),
        restricted: super::job_staff::restricted(t.effective),
    };
    let me = t.caller.sub.clone();
    let mut tx = t.begin(&st.db).await?;
    let mut rows = list(&mut tx, &me, tab, &filters).await?;
    let counts = counts(&mut tx, &me, filters.restricted).await?;
    tx.commit().await?;
    mark_unread(&st, &me, &mut rows).await;
    Ok(Json(json!({ "rows": rows, "counts": counts })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tabs_parse_and_default_to_open() {
        assert_eq!(Tab::parse(None).unwrap(), Tab::Open);
        assert_eq!(Tab::parse(Some(" ")).unwrap(), Tab::Open);
        assert_eq!(Tab::parse(Some("mine")).unwrap(), Tab::Mine);
        assert_eq!(Tab::parse(Some("unassigned")).unwrap(), Tab::Unassigned);
        assert_eq!(Tab::parse(Some("resolved")).unwrap(), Tab::Resolved);
        assert_eq!(Tab::parse(Some("all")).unwrap_err().code(), "invalid_query");
    }

    #[test]
    fn a_reason_filter_takes_the_code_or_the_label() {
        assert_eq!(reason_codes("customer_asked"), vec!["customer_asked"]);
        assert_eq!(reason_codes("Customer asked"), vec!["customer_asked"]);
        assert!(reason_codes("nope").is_empty());
    }
}
