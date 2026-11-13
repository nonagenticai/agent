//! Conversation-state fixtures (A1): a conversation's identity lives in `conversation_state`, not in columns. These
//! helpers write and read it through the OWNER pool (RLS does not apply to the table owner), exactly the rows the
//! channels write in production (`agent_api::conversations::state`).
use agent_api::conversations::state as st;
use sqlx::PgPool;
use uuid::Uuid;

/// Set one plain state `key` of `conversation` (replacing any earlier entry).
pub async fn put(
    owner: &PgPool,
    tenant: Uuid,
    job: Uuid,
    conversation: &str,
    key: &str,
    value: &str,
) {
    sqlx::query(
        "INSERT INTO conversation_state \
            (tenant_id, job_id, conversation_id, key, kind, value, produced_by, updated_at) \
         VALUES ($1, $2, $3, $4, 'value', $5, 'test', now()) \
         ON CONFLICT (tenant_id, job_id, conversation_id, key) DO UPDATE \
            SET kind = 'value', value = EXCLUDED.value, secret = NULL, expires_at = NULL, updated_at = now()",
    )
    .bind(tenant)
    .bind(job)
    .bind(conversation)
    .bind(key)
    .bind(value)
    .execute(owner)
    .await
    .expect("write conversation state");
}

/// Make `conversation`'s customer known the way the web chat does: `subject`, optional `email`, and
/// `identity_source = assertion`.
pub async fn identify(
    owner: &PgPool,
    tenant: Uuid,
    job: Uuid,
    conversation: &str,
    subject: &str,
    email: Option<&str>,
) {
    identify_as(
        owner,
        tenant,
        job,
        conversation,
        st::SOURCE_ASSERTION,
        subject,
        email,
    )
    .await;
}

/// [`identify`] with an explicit `identity_source` (`assertion` | `staff_pick` | `staff_test`).
pub async fn identify_as(
    owner: &PgPool,
    tenant: Uuid,
    job: Uuid,
    conversation: &str,
    source: &str,
    subject: &str,
    email: Option<&str>,
) {
    put(owner, tenant, job, conversation, st::SUBJECT, subject).await;
    if let Some(e) = email {
        put(owner, tenant, job, conversation, st::EMAIL, e).await;
    }
    put(
        owner,
        tenant,
        job,
        conversation,
        st::IDENTITY_SOURCE,
        source,
    )
    .await;
}

/// The identity a case stands at, derived from its conversation's state: `(identity level, verified party, email)`
/// (what the dropped `agent_case.identity_level / verified_party / verified_email` held).
pub async fn case_identity(
    owner: &PgPool,
    tenant: Uuid,
    job: Uuid,
    case: &str,
) -> (i32, Option<String>, Option<String>) {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {level}, CASE WHEN {known} THEN {subject} END, CASE WHEN {known} THEN {email} END \
           FROM agent_case ac WHERE ac.tenant_id = $1 AND ac.job_id = $2 AND ac.case_id = $3",
        level = st::level_sql("ac"),
        known = st::known_sql("ac"),
        subject = st::sub("ac", st::SUBJECT),
        email = st::sub("ac", st::EMAIL),
    )))
    .bind(tenant)
    .bind(job)
    .bind(case)
    .fetch_one(owner)
    .await
    .expect("read a case's identity")
}

/// The plain value of state `key` of `conversation`, if present.
pub async fn get(
    owner: &PgPool,
    tenant: Uuid,
    job: Uuid,
    conversation: &str,
    key: &str,
) -> Option<String> {
    sqlx::query_scalar::<_, Option<String>>(
        "SELECT value FROM conversation_state \
          WHERE tenant_id = $1 AND job_id = $2 AND conversation_id = $3 AND key = $4",
    )
    .bind(tenant)
    .bind(job)
    .bind(conversation)
    .bind(key)
    .fetch_optional(owner)
    .await
    .expect("read conversation state")
    .flatten()
}
