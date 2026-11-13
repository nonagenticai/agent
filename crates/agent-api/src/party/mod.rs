//! Party identity (B13 §1, B9 T3/T9): who a customer is across channels (0112).
//!
//! - [`resolve`]: every inbound message names a channel subject (the wire `party.id`); the subject's identity row is
//!   upserted and its party answered: a new subject gets a new party, once (an advisory lock per subject
//!   serialises first contact, `ON CONFLICT` backs it up). A merged party answers as the party it was merged into.
//! - [`attach`]: the conversation (and its cases and items still without one) get the party.
//! - [`link`]: staff link a case to a party (`linkCaseParty`); a party with no identified customer is merged into the target. A link is an act on one job: only that job's rows move, and the source party
//!   is marked merged only once no other job's rows still name it.
//! - views: [`brief`] (`{id, known, subject, display_name}`; `subject` = the identified customer of its newest known
//!   conversation, from conversation state, `known` = there is one), [`full`] (identities, cases,
//!   `merged_into`; the cases of one job, of every job, or none — see [`Cases`]).
//!
//! Tenant-wide, no job scope: the merge and the reads across jobs run in a transaction with the job GUC empty; a
//! job-scoped route asks [`full`] for its own job's cases only.
//! Subjects and names are customer data: never in a log line, an audit detail or a metric label.
pub mod routes;

use serde_json::{Value, json};
use sqlx::{PgConnection, Row};
use uuid::Uuid;

use crate::conversations::{CHANNELS, iso, store};
use crate::error::{ApiError, ApiResult};

/// A `party.id`.
pub type PartyId = Uuid;

/// The longest stored display name (characters).
pub const MAX_DISPLAY_NAME: usize = 120;

/// `merged_into` hops followed before giving up (merges re-point earlier merges, so one hop is the norm).
const MAX_MERGE_HOPS: usize = 8;

/// The party `id` answers as: itself, or the end of its `merged_into` chain.
pub async fn current(conn: &mut PgConnection, id: PartyId) -> ApiResult<PartyId> {
    let mut id = id;
    for _ in 0..MAX_MERGE_HOPS {
        let next: Option<Option<Uuid>> =
            sqlx::query_scalar("SELECT merged_into FROM party WHERE id = $1")
                .bind(id)
                .fetch_optional(&mut *conn)
                .await?;
        match next.flatten() {
            Some(n) => id = n,
            None => return Ok(id),
        }
    }
    Ok(id)
}

/// The party behind `subject` on `channel` (see the module doc). `display_name` fills an empty name; `verified` marks
/// the identity verified (the party stays `anonymous` until staff [`link`] it). Runs in the
/// caller's transaction (tenant GUC set).
pub async fn resolve(
    conn: &mut PgConnection,
    tenant: Uuid,
    channel: &str,
    subject: &str,
    display_name: Option<&str>,
    verified: bool,
) -> ApiResult<PartyId> {
    if !CHANNELS.contains(&channel) {
        return Err(ApiError::Unprocessable(
            "invalid_channel",
            "channel is not a known channel".into(),
        ));
    }
    let name: Option<String> = display_name
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(|n| n.chars().take(MAX_DISPLAY_NAME).collect());
    store::lock(conn, &format!("party:{tenant}:{channel}:{subject}")).await?;
    // `now()` is the transaction start: a caller that waited on the lock behind one that inserted the identity has
    // an earlier `now()` than its `first_seen_at`, so `last_seen_at` only moves forward (party_identity_seen_check).
    let found: Option<Uuid> = sqlx::query_scalar(
        "UPDATE party_identity SET last_seen_at = greatest(last_seen_at, now()), verified = verified OR $4 \
          WHERE tenant_id = $1 AND channel = $2 AND subject = $3 RETURNING party_id",
    )
    .bind(tenant)
    .bind(channel)
    .bind(subject)
    .bind(verified)
    .fetch_optional(&mut *conn)
    .await?;
    let id = match found {
        Some(p) => p,
        None => {
            let new_id = Uuid::now_v7();
            sqlx::query(
                "INSERT INTO party (id, tenant_id, display_name, created_at, updated_at) \
                 VALUES ($1, $2, $3, now(), now())",
            )
            .bind(new_id)
            .bind(tenant)
            .bind(name.as_deref())
            .execute(&mut *conn)
            .await?;
            sqlx::query_scalar(
                "INSERT INTO party_identity \
                    (tenant_id, channel, subject, party_id, verified, first_seen_at, last_seen_at) \
                 VALUES ($1, $2, $3, $4, $5, now(), now()) \
                 ON CONFLICT (tenant_id, channel, subject) \
                 DO UPDATE SET last_seen_at = greatest(party_identity.last_seen_at, now()), \
                               verified = party_identity.verified OR EXCLUDED.verified \
                 RETURNING party_id",
            )
            .bind(tenant)
            .bind(channel)
            .bind(subject)
            .bind(new_id)
            .bind(verified)
            .fetch_one(&mut *conn)
            .await?
        }
    };
    let id = current(conn, id).await?;
    sqlx::query(
        "UPDATE party SET display_name = COALESCE(display_name, $2), updated_at = now() \
          WHERE id = $1 AND display_name IS NULL AND $2::text IS NOT NULL",
    )
    .bind(id)
    .bind(name.as_deref())
    .execute(&mut *conn)
    .await?;
    Ok(id)
}

/// Give the conversation, and its cases and handover items that have none yet, the party `party`.
pub async fn attach(
    conn: &mut PgConnection,
    conversation_id: &str,
    party: PartyId,
) -> ApiResult<()> {
    for sql in [
        "UPDATE conversation SET party_id = $2 WHERE conversation_id = $1 AND party_id IS NULL",
        "UPDATE agent_case SET party_id = $2 WHERE conversation_id = $1 AND party_id IS NULL",
        "UPDATE agent_handover_items SET party_id = $2 WHERE conversation_id = $1 AND party_id IS NULL",
    ] {
        sqlx::query(sql)
            .bind(conversation_id)
            .bind(party)
            .execute(&mut *conn)
            .await?;
    }
    Ok(())
}

/// Merge `source` into `target` (both locked by the caller, `target` not merged) on behalf of job `job_id`: that
/// job's conversations, cases and items of `source` are relinked to `target`. Only when `source` then has no row
/// left in any other job is it marked merged: `source.merged_into`, earlier merges into `source` re-pointed,
/// identities moved. Otherwise `source` stays a party of its own (identities included): a support agent of one job
/// does not re-point another job's customers. Answers whether `source` was marked merged. The transaction must have
/// the job GUC empty (the remaining-rows check sees every job).
pub async fn merge(
    conn: &mut PgConnection,
    source: PartyId,
    target: PartyId,
    job_id: Uuid,
) -> ApiResult<bool> {
    for sql in [
        "UPDATE conversation SET party_id = $2 WHERE party_id = $1 AND job_id = $3",
        "UPDATE agent_case SET party_id = $2 WHERE party_id = $1 AND job_id = $3",
        "UPDATE agent_handover_items SET party_id = $2 WHERE party_id = $1 AND job_id = $3",
    ] {
        sqlx::query(sql)
            .bind(source)
            .bind(target)
            .bind(job_id)
            .execute(&mut *conn)
            .await?;
    }
    let elsewhere: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM conversation WHERE party_id = $1) \
             OR EXISTS (SELECT 1 FROM agent_case WHERE party_id = $1) \
             OR EXISTS (SELECT 1 FROM agent_handover_items WHERE party_id = $1)",
    )
    .bind(source)
    .fetch_one(&mut *conn)
    .await?;
    if elsewhere {
        return Ok(false);
    }
    for sql in [
        "UPDATE party SET merged_into = $2, updated_at = now() WHERE merged_into = $1",
        "UPDATE party SET merged_into = $2, updated_at = now() WHERE id = $1",
        "UPDATE party t SET display_name = s.display_name, updated_at = now() \
           FROM party s WHERE t.id = $2 AND s.id = $1 AND t.display_name IS NULL AND s.display_name IS NOT NULL",
        "UPDATE party_identity SET party_id = $2 WHERE party_id = $1",
    ] {
        sqlx::query(sql)
            .bind(source)
            .bind(target)
            .execute(&mut *conn)
            .await?;
    }
    Ok(true)
}

/// SQL: the identified customer's `subject` for the party row aliased `alias` — the subject state of its newest
/// conversation whose customer is known (`assertion` | `staff_pick` | `staff_test`); NULL when none (the party is
/// "anonymous"). Replaces the dropped `party.kind` / `party.external_customer_ref`; a reader sees only the
/// conversations its transaction can see (a job-scoped route: its own job's).
pub fn subject_sql(alias: &str) -> String {
    use crate::conversations::state;
    format!(
        "(SELECT {subject} FROM conversation cv WHERE cv.tenant_id = {alias}.tenant_id AND cv.party_id = {alias}.id \
            AND {known} ORDER BY cv.started_at DESC, cv.conversation_id DESC LIMIT 1)",
        subject = state::sub("cv", state::SUBJECT),
        known = state::known_sql("cv"),
    )
}

pub fn not_found() -> ApiError {
    ApiError::Missing("party_not_found", "No such party in this tenant.".into())
}

/// What [`link`] did.
#[derive(Clone, Debug)]
pub struct Linked {
    pub from: Option<PartyId>,
    pub merged: bool,
    pub changed: bool,
}

/// The lock order of a link, step one: the control lock and owner row ([`ownership::guard`]) of every conversation
/// whose handover items [`link`] (and [`merge`]) will update, taken BEFORE any case / item / party row lock. Hand back
/// and Resolve lock item then case under the same guard, so a link that took the case row first and the item second
/// deadlocked with them. The conversations are the case's own plus, when the case has a party, every conversation of
/// this job that names that party on a conversation, case or item row (the rows [`merge`] relinks), sorted ascending
/// and de-duplicated so concurrent links and commands take the guards in one order. The party is read unlocked: a
/// case relinked between this read and the case lock in [`link`] (a concurrent link of the same case) may leave
/// a conversation of the new source unguarded; that residual window needs two concurrent links of one case AND a
/// command on a third conversation of the merged party, and is not closed here. The transaction must have the job
/// GUC empty (like [`link`]). A missing case guards nothing ([`link`] answers 404).
pub async fn guard_link(conn: &mut PgConnection, job_id: Uuid, case_id: &str) -> ApiResult<()> {
    let Some((conversation_id, party_id)): Option<(String, Option<Uuid>)> = sqlx::query_as(
        "SELECT conversation_id, party_id FROM agent_case WHERE job_id = $1 AND case_id = $2",
    )
    .bind(job_id)
    .bind(case_id)
    .fetch_optional(&mut *conn)
    .await?
    else {
        return Ok(());
    };
    let mut ids: Vec<String> = vec![conversation_id];
    if let Some(p) = party_id {
        let more: Vec<String> = sqlx::query_scalar(
            "SELECT conversation_id FROM conversation WHERE party_id = $1 AND job_id = $2 \
             UNION SELECT conversation_id FROM agent_case WHERE party_id = $1 AND job_id = $2 \
             UNION SELECT conversation_id FROM agent_handover_items WHERE party_id = $1 AND job_id = $2",
        )
        .bind(p)
        .bind(job_id)
        .fetch_all(&mut *conn)
        .await?;
        ids.extend(more);
    }
    ids.sort();
    ids.dedup();
    for id in &ids {
        crate::conversations::ownership::guard(&mut *conn, id).await?;
    }
    Ok(())
}

/// `linkCaseParty`: case `case_id` of job `job_id` (and its handover items) now belong to `target`; the case's previous party is merged into it (see [`merge`]: this job's rows, and the party itself only
/// when no other job's rows name it) when it has no identified customer (no known `subject` in the state of its conversations). 404
/// `case_not_found`, 404 `party_not_found`, 409 `party_merged` (the target was merged into another party). The
/// transaction must have the job GUC empty, and the caller must have run [`guard_link`] first (lock order: control
/// lock and owner row of each affected conversation, then the case row, then items).
pub async fn link(
    conn: &mut PgConnection,
    job_id: Uuid,
    case_id: &str,
    target: PartyId,
) -> ApiResult<Linked> {
    let row: Option<Option<Uuid>> = sqlx::query_scalar(
        "SELECT party_id FROM agent_case WHERE job_id = $1 AND case_id = $2 FOR UPDATE",
    )
    .bind(job_id)
    .bind(case_id)
    .fetch_optional(&mut *conn)
    .await?;
    let from =
        row.ok_or_else(|| ApiError::Missing("case_not_found", "No such case in this job.".into()))?;
    // both parties, in id order (one lock order for concurrent links)
    let mut ids = vec![target];
    ids.extend(from.filter(|f| *f != target));
    ids.sort();
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT p.id, {subject} AS subject, p.merged_into FROM party p WHERE p.id = ANY($1) \
          ORDER BY p.id FOR UPDATE OF p",
        subject = subject_sql("p"),
    )))
    .bind(&ids)
    .fetch_all(&mut *conn)
    .await?;
    let t = rows
        .iter()
        .find(|r| r.get::<Uuid, _>("id") == target)
        .ok_or_else(not_found)?;
    if t.get::<Option<Uuid>, _>("merged_into").is_some() {
        return Err(ApiError::Conflict(
            "party_merged",
            "That party was merged into another party; link the case to that one.".into(),
        ));
    }
    if from == Some(target) {
        return Ok(Linked {
            from,
            merged: false,
            changed: false,
        });
    }
    let source = from.and_then(|f| rows.iter().find(|r| r.get::<Uuid, _>("id") == f));
    let mergeable = source.is_some_and(|s| {
        s.get::<Option<String>, _>("subject").is_none()
            && s.get::<Option<Uuid>, _>("merged_into").is_none()
    });
    let mut merged = false;
    if let (true, Some(f)) = (mergeable, from) {
        merged = merge(conn, f, target, job_id).await?;
    }
    for sql in [
        "UPDATE agent_case SET party_id = $3, updated_at = now() WHERE job_id = $1 AND case_id = $2",
        "UPDATE agent_handover_items SET party_id = $3 WHERE job_id = $1 AND case_id = $2",
    ] {
        sqlx::query(sql)
            .bind(job_id)
            .bind(case_id)
            .bind(target)
            .execute(&mut *conn)
            .await?;
    }
    Ok(Linked {
        from,
        merged,
        changed: true,
    })
}

/// `{id, known, subject, display_name}` of `id`, or `null` (no party, or not visible).
pub async fn brief(conn: &mut PgConnection, id: Option<PartyId>) -> ApiResult<Value> {
    let Some(id) = id else {
        return Ok(Value::Null);
    };
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT p.id, p.display_name, {subject} AS subject FROM party p WHERE p.id = $1",
        subject = subject_sql("p"),
    )))
    .bind(id)
    .fetch_optional(conn)
    .await?;
    Ok(row.map_or(Value::Null, |r| {
        let subject = r.get::<Option<String>, _>("subject");
        json!({
            "id": r.get::<Uuid, _>("id"),
            "known": subject.is_some(),
            "subject": subject,
            "display_name": r.get::<Option<String>, _>("display_name"),
        })
    }))
}

/// The most cases [`full`] lists.
pub const CASES_LIMIT: i64 = 50;

/// Which cases [`full`] lists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cases {
    /// No `cases` key.
    Omit,
    /// The party's cases of this job only (job-scoped routes: a member of one job sees no other job's cases).
    Job(Uuid),
    /// The party's cases of every job the transaction sees (the tenant-level support_agent routes).
    Tenant,
}

/// One party with its identities (and its newest cases, per `cases`).
pub async fn full(conn: &mut PgConnection, id: PartyId, cases: Cases) -> ApiResult<Option<Value>> {
    let Some(r) = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT p.id, p.display_name, {subject} AS subject, p.merged_into, p.created_at, p.updated_at \
           FROM party p WHERE p.id = $1",
        subject = subject_sql("p"),
    )))
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?
    else {
        return Ok(None);
    };
    let identities: Vec<Value> = sqlx::query(
        "SELECT channel, subject, verified, first_seen_at, last_seen_at FROM party_identity \
          WHERE party_id = $1 ORDER BY first_seen_at, channel, subject",
    )
    .bind(id)
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .map(|i| {
        json!({
            "channel": i.get::<String, _>("channel"),
            "subject": i.get::<String, _>("subject"),
            "verified": i.get::<bool, _>("verified"),
            "first_seen_at": iso(i.get("first_seen_at")),
            "last_seen_at": iso(i.get("last_seen_at")),
        })
    })
    .collect();
    let subject = r.get::<Option<String>, _>("subject");
    let mut v = json!({
        "id": r.get::<Uuid, _>("id"),
        "known": subject.is_some(),
        "subject": subject,
        "display_name": r.get::<Option<String>, _>("display_name"),
        "merged_into": r.get::<Option<Uuid>, _>("merged_into"),
        "created_at": iso(r.get("created_at")),
        "updated_at": iso(r.get("updated_at")),
        "identities": identities,
    });
    if cases != Cases::Omit {
        let job: Option<Uuid> = match cases {
            Cases::Job(j) => Some(j),
            _ => None,
        };
        let cs: Vec<Value> = sqlx::query(
            "SELECT j.key AS job, a.case_id, a.conversation_id, a.channel, a.status, a.title, a.started_at \
               FROM agent_case a JOIN jobs j ON j.id = a.job_id \
              WHERE a.party_id = $1 AND ($3::uuid IS NULL OR a.job_id = $3) \
              ORDER BY a.started_at DESC, a.case_id DESC LIMIT $2",
        )
        .bind(id)
        .bind(CASES_LIMIT)
        .bind(job)
        .fetch_all(&mut *conn)
        .await?
        .into_iter()
        .map(|c| {
            json!({
                "job": c.get::<String, _>("job"),
                "case_id": c.get::<String, _>("case_id"),
                "conversation_id": c.get::<String, _>("conversation_id"),
                "channel": c.get::<String, _>("channel"),
                "status": c.get::<String, _>("status"),
                "title": c.get::<String, _>("title"),
                "started_at": iso(c.get("started_at")),
            })
        })
        .collect();
        v["cases"] = Value::Array(cs);
    }
    Ok(Some(v))
}
