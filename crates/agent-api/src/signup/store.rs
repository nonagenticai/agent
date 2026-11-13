//! `signup_approvals` and `signup_provisioning_steps` (migration 0030). Ported from strata_configurator
//! `db/signups.rs`. Platform tables: no RLS, granted to `agent_platform` only (migration 0031); every caller is
//! a guarded route or the provisioning tick, and every function here takes a connection that is
//! `agent_platform` at the time (crate::platform_db `begin_platform`, or between `enter` and `leave`).
use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::PgConnection;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Approval {
    pub id: Uuid,
    pub email: String,
    pub name: String,
    pub company: Option<String>,
    pub message: Option<String>,
    /// `web`, `chat` or `invite` (a super_admin's pre-approved invite).
    pub source: String,
    /// One of the eight signup locales, or null (en).
    pub locale: Option<String>,
    /// `intake`; null for an invite.
    pub trust: Option<String>,
    /// The intake client that submitted it; null for an invite.
    pub intake_client: Option<String>,
    /// The super_admin who sent an invite; null for an intake row.
    pub invited_by: Option<String>,
    /// `pending`, `approved` or `rejected`.
    pub status: String,
    /// The tenant the approval created.
    pub tenant_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub decided_at: Option<DateTime<Utc>>,
    pub decided_by: Option<String>,
    /// `ui` or `telegram`.
    pub decided_via: Option<String>,
    pub reason: Option<String>,
    /// An approve is running on it right now (its claim is live): a reject or a second approve waits.
    pub approving: bool,
    /// `created_at` in microseconds since the epoch: the paging cursor, not part of the body.
    #[serde(skip)]
    pub ts_us: i64,
}

const COLUMNS: &str = "id, email, name, company, message, source, locale, trust, intake_client, invited_by, status, \
                       approved_tenant_id AS tenant_id, created_at, decided_at, decided_by, decided_via, reason, \
                       (approving_until IS NOT NULL AND approving_until > now()) AS approving, \
                       (extract(epoch FROM created_at) * 1000000)::bigint AS ts_us";

/// What the intake records.
pub struct NewSignup<'a> {
    pub email: &'a str,
    pub name: &'a str,
    pub company: Option<&'a str>,
    pub message: Option<&'a str>,
    pub source: &'a str,
    pub locale: Option<&'a str>,
    pub intake_client: &'a str,
}

/// The transaction-scoped advisory lock key that serialises intakes, so the pending-queue ceiling is a hard
/// cap: under READ COMMITTED two concurrent intakes could otherwise both count below it and both insert.
pub const QUEUE_LOCK: i64 = 0x5167_a0e1;

pub async fn lock_queue(conn: &mut PgConnection) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(QUEUE_LOCK)
        .execute(conn)
        .await?;
    Ok(())
}

pub async fn pending_count(conn: &mut PgConnection) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM signup_approvals WHERE status = 'pending'")
        .fetch_one(conn)
        .await
}

/// Queue a pending signup; `None` when a pending row for the address (case-insensitively) already exists,
/// which is left as it is (the partial unique index; a decided row does not block).
pub async fn insert(
    conn: &mut PgConnection,
    s: &NewSignup<'_>,
) -> Result<Option<Uuid>, sqlx::Error> {
    sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO signup_approvals (email, name, company, message, source, locale, intake_client) \
         VALUES ($1, $2, $3, $4, $5, $6, $7) ON CONFLICT DO NOTHING RETURNING id",
    )
    .bind(s.email)
    .bind(s.name)
    .bind(s.company)
    .bind(s.message)
    .bind(s.source)
    .bind(s.locale)
    .bind(s.intake_client)
    .fetch_optional(conn)
    .await
}

/// The queue, newest first, one page. `status`: a status, `decided` (approved or rejected), or all.
pub async fn list(
    conn: &mut PgConnection,
    status: Option<&str>,
    before: Option<(i64, Uuid)>,
    fetch: i64,
) -> Result<Vec<Approval>, sqlx::Error> {
    let (us, id) = match before {
        Some((us, id)) => (Some(us), Some(id)),
        None => (None, None),
    };
    sqlx::query_as::<_, Approval>(sqlx::AssertSqlSafe(format!(
        "SELECT {COLUMNS} FROM signup_approvals \
         WHERE ($4::text IS NULL OR status = $4 OR ($4 = 'decided' AND status <> 'pending')) \
           AND ($1::bigint IS NULL \
                OR (created_at, id) < (timestamptz 'epoch' + $1::bigint * interval '1 microsecond', $2::uuid)) \
         ORDER BY created_at DESC, id DESC LIMIT $3"
    )))
    .bind(us)
    .bind(id)
    .bind(fetch)
    .bind(status)
    .fetch_all(conn)
    .await
}

/// (received, pending, approved, rejected) over every signup.
pub async fn counts(conn: &mut PgConnection) -> Result<(i64, i64, i64, i64), sqlx::Error> {
    sqlx::query_as(
        "SELECT count(*), count(*) FILTER (WHERE status = 'pending'), \
                count(*) FILTER (WHERE status = 'approved'), count(*) FILTER (WHERE status = 'rejected') \
           FROM signup_approvals",
    )
    .fetch_one(conn)
    .await
}

pub async fn get(conn: &mut PgConnection, id: Uuid) -> Result<Option<Approval>, sqlx::Error> {
    sqlx::query_as::<_, Approval>(sqlx::AssertSqlSafe(format!(
        "SELECT {COLUMNS} FROM signup_approvals WHERE id = $1"
    )))
    .bind(id)
    .fetch_optional(conn)
    .await
}

/// The signup `id` locked for the rest of the transaction (any status); `None` when there is none. A
/// concurrent decision blocks here until this transaction ends, then reads the committed status.
pub async fn lock(conn: &mut PgConnection, id: Uuid) -> Result<Option<Approval>, sqlx::Error> {
    sqlx::query_as::<_, Approval>(sqlx::AssertSqlSafe(format!(
        "SELECT {COLUMNS} FROM signup_approvals WHERE id = $1 FOR UPDATE"
    )))
    .bind(id)
    .fetch_optional(conn)
    .await
}

/// Mark the PENDING signup `id` approved into `tenant`, only while the approve claim `token` holds it (and the claim
/// is released in the same statement); `None` when it is not pending any more or the claim was lost.
pub async fn mark_approved(
    conn: &mut PgConnection,
    id: Uuid,
    tenant: Uuid,
    by: &str,
    via: &str,
    token: Uuid,
) -> Result<Option<Approval>, sqlx::Error> {
    sqlx::query_as::<_, Approval>(sqlx::AssertSqlSafe(format!(
        "UPDATE signup_approvals \
         SET status = 'approved', approved_tenant_id = $2, decided_at = now(), decided_by = $3, decided_via = $4, \
             approving_until = NULL, approving_claim = NULL \
         WHERE id = $1 AND status = 'pending' AND approving_claim = $5 RETURNING {COLUMNS}"
    )))
    .bind(id)
    .bind(tenant)
    .bind(by)
    .bind(via)
    .bind(token)
    .fetch_optional(conn)
    .await
}

/// Take the approve claim on the PENDING signup `id` (commonpractice `claimApproval`): one conditional UPDATE with a
/// fresh token, only when no live claim holds it. `None` when it is not pending or another approver holds it.
pub async fn claim(
    conn: &mut PgConnection,
    id: Uuid,
    token: Uuid,
    secs: f64,
) -> Result<Option<Approval>, sqlx::Error> {
    sqlx::query_as::<_, Approval>(sqlx::AssertSqlSafe(format!(
        "UPDATE signup_approvals \
         SET approving_until = now() + make_interval(secs => $3), approving_claim = $2 \
         WHERE id = $1 AND status = 'pending' AND (approving_until IS NULL OR approving_until <= now()) \
         RETURNING {COLUMNS}"
    )))
    .bind(id)
    .bind(token)
    .bind(secs)
    .fetch_optional(conn)
    .await
}

/// Re-confirm and extend the claim `token` on `id` (commonpractice `assertHeld`): only while it is still ours, live
/// and the row pending. `false` = the claim is lost.
pub async fn hold(
    conn: &mut PgConnection,
    id: Uuid,
    token: Uuid,
    secs: f64,
) -> Result<bool, sqlx::Error> {
    Ok(sqlx::query(
        "UPDATE signup_approvals SET approving_until = now() + make_interval(secs => $3) \
          WHERE id = $1 AND approving_claim = $2 AND status = 'pending' AND approving_until > now()",
    )
    .bind(id)
    .bind(token)
    .bind(secs)
    .execute(conn)
    .await?
    .rows_affected()
        == 1)
}

/// Release the claim `token` on `id` (only ours: a lapsed claim may have been taken over since).
pub async fn release(conn: &mut PgConnection, id: Uuid, token: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE signup_approvals SET approving_until = NULL, approving_claim = NULL \
          WHERE id = $1 AND approving_claim = $2",
    )
    .bind(id)
    .bind(token)
    .execute(conn)
    .await?;
    Ok(())
}

/// A super_admin's pre-approved invite (`source = 'invite'`, no trust, no intake client); `None` when a pending row
/// for the address already exists (the partial unique index).
pub async fn insert_invite(
    conn: &mut PgConnection,
    email: &str,
    name: &str,
    company: Option<&str>,
    locale: Option<&str>,
    invited_by: &str,
) -> Result<Option<Uuid>, sqlx::Error> {
    sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO signup_approvals (email, name, company, source, locale, trust, intake_client, invited_by) \
         VALUES ($1, $2, $3, 'invite', $4, NULL, NULL, $5) ON CONFLICT DO NOTHING RETURNING id",
    )
    .bind(email)
    .bind(name)
    .bind(company)
    .bind(locale)
    .bind(invited_by)
    .fetch_optional(conn)
    .await
}

/// The PENDING signup of `email` (case-insensitively), if any.
pub async fn pending_of(conn: &mut PgConnection, email: &str) -> Result<Option<Uuid>, sqlx::Error> {
    sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM signup_approvals WHERE lower(email) = lower($1) AND status = 'pending'",
    )
    .bind(email)
    .fetch_optional(conn)
    .await
}

/// Set the language of signup `id` (an invite's choice wins over what an earlier signup carried).
pub async fn set_locale(
    conn: &mut PgConnection,
    id: Uuid,
    locale: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE signup_approvals SET locale = $2 WHERE id = $1")
        .bind(id)
        .bind(locale)
        .execute(conn)
        .await?;
    Ok(())
}

/// Mark the PENDING signup `id` rejected; `None` when it is not pending (any more). The caller has checked, under the
/// row lock, that no approve claim is live.
pub async fn mark_rejected(
    conn: &mut PgConnection,
    id: Uuid,
    by: &str,
    via: &str,
    reason: Option<&str>,
) -> Result<Option<Approval>, sqlx::Error> {
    sqlx::query_as::<_, Approval>(sqlx::AssertSqlSafe(format!(
        "UPDATE signup_approvals \
         SET status = 'rejected', decided_at = now(), decided_by = $2, decided_via = $3, reason = $4, \
             approving_until = NULL, approving_claim = NULL \
         WHERE id = $1 AND status = 'pending' RETURNING {COLUMNS}"
    )))
    .bind(id)
    .bind(by)
    .bind(via)
    .bind(reason)
    .fetch_optional(conn)
    .await
}

/// One journal row (`signup_provisioning_steps`), as the API shows it.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct StepRow {
    pub step: String,
    pub ord: i16,
    pub status: String,
    pub attempts: i32,
    pub code: Option<String>,
    pub message: Option<String>,
    /// Internal (Keycloak sub, licence id): never serialised.
    #[serde(skip)]
    pub data: serde_json::Value,
    pub last_attempt_at: Option<DateTime<Utc>>,
    pub next_attempt_at: Option<DateTime<Utc>>,
}

pub async fn steps(conn: &mut PgConnection, id: Uuid) -> Result<Vec<StepRow>, sqlx::Error> {
    sqlx::query_as::<_, StepRow>(
        "SELECT step, ord, status, attempts, code, message, data, last_attempt_at, next_attempt_at \
           FROM signup_provisioning_steps WHERE signup_id = $1 ORDER BY ord",
    )
    .bind(id)
    .fetch_all(conn)
    .await
}

/// The journals of `ids`, in step order.
pub async fn steps_many(
    conn: &mut PgConnection,
    ids: &[Uuid],
) -> Result<Vec<(Uuid, StepRow)>, sqlx::Error> {
    #[derive(sqlx::FromRow)]
    struct Row {
        signup_id: Uuid,
        #[sqlx(flatten)]
        step: StepRow,
    }
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT signup_id, step, ord, status, attempts, code, message, data, last_attempt_at, next_attempt_at \
           FROM signup_provisioning_steps WHERE signup_id = ANY($1) ORDER BY signup_id, ord",
    )
    .bind(ids)
    .fetch_all(conn)
    .await?;
    Ok(rows.into_iter().map(|r| (r.signup_id, r.step)).collect())
}
