//! Conversation state (A1 of the caller identity / IAM plan, 0231): a conversation's key/value state — the channel's
//! identity (the reserved keys below) and whatever a tool declaration produces — and the opt-in per-subject
//! carry-over.
//!
//! - A `value` is plain text; a `secret` is sealed with the [`Keyring`] the caller passes (AAD =
//!   `tenant|conversation|key`) and is opened only by [`get`] (runtime use). [`load`] and [`present`] never open
//!   it: the model, the logs, the audit details and the case events see a key as present or absent, nothing more.
//! - An expired row is absent to every reader ([`is_live`]).
//! - Carry-over: a key a declaration marks `persist_for_subject` is copied by [`remember_for_subject`] to
//!   `subject_state`, keyed by the conversation's subject (`subject:<id>`, else `channel:<id>`), and copied back
//!   into a later conversation of the same subject by [`carry_over_into`] (re-sealed for that conversation).
//!
//! Every function takes the connection of a transaction opened by [`super::store::begin_job`] (RLS-scoped to the
//! tenant and job). Values and secrets are customer data: never in a log line, an audit detail or a metric label.
use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use sqlx::{PgConnection, Row};

use crate::crypto::keyring::Keyring;
use crate::error::{ApiError, ApiResult};

/// The system's own customer id (what `conversation.customer_id` held for a known customer).
pub const SUBJECT: &str = "subject";
/// The known customer's email.
pub const EMAIL: &str = "email";
/// The known customer's display name.
pub const NAME: &str = "name";
/// The wire party id of a channel that does not identify its sender (Telegram, an anonymous web chat session).
pub const CHANNEL_SUBJECT: &str = "channel_subject";
/// An API key caller: `{key_id}`.
pub const CALLER: &str = "caller";
/// The API key's label.
pub const CALLER_LABEL: &str = "caller_label";
/// The latest `callback_url` of an async `/interactions` request on the conversation (validated at intake, read from
/// its `interaction_deliveries` row): where a task message the relay posts later reaches an API caller.
pub const CALLBACK_URL: &str = "callback_url";
/// The fr project an MCP task started for the conversation (`_meta["ai.nonagentic/fr"].project_id`), written by
/// crate::runtime::task_relay the first time the server names it (and again when a task started later names another).
pub const FR_PROJECT_ID: &str = "fr_project_id";
/// The MCP task id that named [`FR_PROJECT_ID`] (`fr_project_task`). It stays a bare id and never JSON, and so does
/// [`FR_PROJECT_ID`]: a tenant `job_tool_policy` maps `fr_project_id` straight into the id input of `fr.close_project`,
/// and the relay reads this task's `created_at` to decide whether a later-started task's project supersedes it.
pub const FR_PROJECT_TASK: &str = "fr_project_task";
/// The conveyor project an MCP task started for the conversation (`_meta["ai.nonagentic/conveyor"].project_id`),
/// written by crate::runtime::task_relay on first sight (and again when a task started later names another). A bare
/// id like [`FR_PROJECT_ID`]: the tenant's `job_tool_policy` maps `conveyor_project_id` into `projects.close.id`.
pub const CONVEYOR_PROJECT_ID: &str = "conveyor_project_id";
/// The MCP task id that named [`CONVEYOR_PROJECT_ID`] (`conveyor_project_task`): a bare id, never JSON, the same rule
/// as [`FR_PROJECT_TASK`].
pub const CONVEYOR_PROJECT_TASK: &str = "conveyor_project_task";
/// How the identity became known: one of the `SOURCE_*` constants.
pub const IDENTITY_SOURCE: &str = "identity_source";
/// What the tenant's IAM procedure established: `identified` (level 1) or `verified` (level 2). Written beside
/// `identity_source = iam`; absent for every other source.
pub const IDENTITY_STRENGTH: &str = "identity_strength";

pub const SOURCE_ASSERTION: &str = "assertion";
pub const SOURCE_STAFF_PICK: &str = "staff_pick";
pub const SOURCE_STAFF_TEST: &str = "staff_test";
/// A tenant IAM procedure established the identity (owner 2026-11-01); [`IDENTITY_STRENGTH`] says what it proved.
pub const SOURCE_IAM: &str = "iam";
pub const SOURCE_API_KEY: &str = "api_key";
pub const SOURCE_CHANNEL: &str = "channel";

/// The strengths a conversation's identity can stand at (the levels a case of it is born at).
pub const STRENGTH_IDENTIFIED: &str = "identified";
pub const STRENGTH_VERIFIED: &str = "verified";

/// The longest value stored (characters; mirrors the table CHECK).
pub const MAX_VALUE: usize = 8_000;

/// SQL: the live plain value of state `key` for the row aliased `alias` (any table with `tenant_id`, `job_id` and
/// `conversation_id`: `conversation`, `agent_case`, `web_chat_session` …) as a scalar subselect. `key` is one of the
/// reserved constants above (never a caller value), so it is inlined. A secret or an expired entry is NULL.
pub fn sub(alias: &str, key: &str) -> String {
    format!(
        "(SELECT s.value FROM conversation_state s WHERE s.tenant_id = {alias}.tenant_id \
         AND s.job_id = {alias}.job_id AND s.conversation_id = {alias}.conversation_id AND s.key = '{key}' \
         AND s.kind = 'value' AND (s.expires_at IS NULL OR s.expires_at > now()))"
    )
}

/// SQL: whether the row aliased `alias` is a VERIFIED customer's: `subject` present with a sourcing
/// `identity_source` — `assertion` | `staff_pick` | `staff_test`, or a tenant IAM procedure of strength `verified`.
/// An IAM `identified` conversation is known at level 1, not verified: `known_sql` stays false for it, so the
/// verified-only reads (KYC context, party matching) do not run. Mirrors [`known_subject`].
pub fn known_sql(alias: &str) -> String {
    format!(
        "(COALESCE({source} IN ('assertion', 'staff_pick', 'staff_test') \
         OR ({source} = 'iam' AND {strength} = 'verified'), false) \
         AND COALESCE(btrim({subject}), '') <> '')",
        source = sub(alias, IDENTITY_SOURCE),
        strength = sub(alias, IDENTITY_STRENGTH),
        subject = sub(alias, SUBJECT),
    )
}

/// SQL: the identity level the row aliased `alias` stands at (`int4`): 2 for a verified customer
/// ([`crate::conversations::store::KNOWN_CUSTOMER_IDENTITY`]), 1 for a tenant IAM procedure of strength
/// `identified`, else 0. A case's level is derived from its conversation's state, never stored.
pub fn level_sql(alias: &str) -> String {
    format!(
        "(CASE WHEN {known} THEN 2 \
          WHEN COALESCE({source} = 'iam' AND {strength} = 'identified', false) \
               AND COALESCE(btrim({subject}), '') <> '' THEN 1 \
          ELSE 0 END)::int4",
        known = known_sql(alias),
        source = sub(alias, IDENTITY_SOURCE),
        strength = sub(alias, IDENTITY_STRENGTH),
        subject = sub(alias, SUBJECT),
    )
}

/// A reserved key is written by the platform, never by a declaration.
pub fn is_reserved(key: &str) -> bool {
    matches!(
        key,
        SUBJECT
            | EMAIL
            | NAME
            | CHANNEL_SUBJECT
            | CALLER
            | CALLER_LABEL
            | IDENTITY_SOURCE
            | IDENTITY_STRENGTH
            | CALLBACK_URL
            | FR_PROJECT_ID
            | FR_PROJECT_TASK
            | CONVEYOR_PROJECT_ID
            | CONVEYOR_PROJECT_TASK
    )
}

/// `^[a-z][a-z0-9_]{0,63}$` (the table CHECK).
pub fn valid_key(key: &str) -> bool {
    let b = key.as_bytes();
    !b.is_empty()
        && b.len() <= 64
        && b[0].is_ascii_lowercase()
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_')
}

/// A state entry as a reader sees it. A secret is never opened here.
#[derive(Clone, PartialEq, Eq)]
pub enum StateValue {
    Value(String),
    Secret,
}

impl std::fmt::Debug for StateValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StateValue::Value(_) => f.write_str("Value(..)"),
            StateValue::Secret => f.write_str("Secret"),
        }
    }
}

impl StateValue {
    /// The plain value; `None` for a secret.
    pub fn as_value(&self) -> Option<&str> {
        match self {
            StateValue::Value(v) => Some(v),
            StateValue::Secret => None,
        }
    }
}

/// A row is live while it has no expiry or the expiry is in the future.
pub fn is_live(expires_at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> bool {
    expires_at.is_none_or(|e| e > now)
}

/// The verified customer of a state map: `subject` present (a plain value) with `identity_source` one of
/// `assertion` / `staff_pick` / `staff_test`, or `iam` of strength `verified`. A channel's claim never counts.
pub fn known_subject(state: &BTreeMap<String, StateValue>) -> Option<&str> {
    let source = state.get(IDENTITY_SOURCE).and_then(StateValue::as_value);
    let iam_verified = source == Some(SOURCE_IAM)
        && state.get(IDENTITY_STRENGTH).and_then(StateValue::as_value) == Some(STRENGTH_VERIFIED);
    let sourced = matches!(
        source,
        Some(SOURCE_ASSERTION | SOURCE_STAFF_PICK | SOURCE_STAFF_TEST)
    ) || iam_verified;
    let subject = state.get(SUBJECT).and_then(StateValue::as_value)?.trim();
    (sourced && !subject.is_empty()).then_some(subject)
}

/// The person the state names at level 1 or above: `subject` present with a sourcing `identity_source`
/// (`assertion` / `staff_pick` / `staff_test`, or `iam` of either strength). An [`identified_subject`] is a known
/// person, not a verified customer: [`known_subject`] stays the verified rule.
pub fn identified_subject(state: &BTreeMap<String, StateValue>) -> Option<&str> {
    let source = state.get(IDENTITY_SOURCE).and_then(StateValue::as_value);
    let iam = source == Some(SOURCE_IAM)
        && matches!(
            state.get(IDENTITY_STRENGTH).and_then(StateValue::as_value),
            Some(STRENGTH_IDENTIFIED | STRENGTH_VERIFIED)
        );
    let sourced = matches!(
        source,
        Some(SOURCE_ASSERTION | SOURCE_STAFF_PICK | SOURCE_STAFF_TEST)
    ) || iam;
    let subject = state.get(SUBJECT).and_then(StateValue::as_value)?.trim();
    (sourced && !subject.is_empty()).then_some(subject)
}

fn check_key(key: &str) -> ApiResult<()> {
    if valid_key(key) {
        Ok(())
    } else {
        Err(ApiError::Unprocessable(
            "invalid_state_key",
            "A state key is lower-case letters, digits and underscores, starting with a letter (64 at most).".into(),
        ))
    }
}

fn check_value(value: &str) -> ApiResult<()> {
    if value.is_empty() || value.chars().count() > MAX_VALUE {
        return Err(ApiError::Unprocessable(
            "invalid_state_value",
            "A state value is 1 to 8000 characters.".into(),
        ));
    }
    Ok(())
}

fn expiry(ttl_s: Option<i64>) -> Option<DateTime<Utc>> {
    ttl_s.map(|s| Utc::now() + chrono::Duration::seconds(s.clamp(1, 10 * 365 * 86_400)))
}

fn unavailable(what: &'static str) -> ApiError {
    ApiError::Internal(anyhow::anyhow!("conversation state: {what}"))
}

/// The AAD scope of a secret: `tenant|conversation|key`.
fn aad(tenant: &str, scope: &str, key: &str) -> Vec<u8> {
    format!("{tenant}|{scope}|{key}").into_bytes()
}

async fn tenant_text(conn: &mut PgConnection) -> ApiResult<String> {
    let t: Option<String> = sqlx::query_scalar("SELECT app_current_tenant()::text")
        .fetch_one(conn)
        .await?;
    t.ok_or_else(|| unavailable("no tenant scope"))
}

/// Set a plain `value` for `key` of `conversation`, replacing any earlier entry (value or secret). `ttl_s` = seconds
/// until it expires; `produced_by` = `channel:<channel>`, `staff:<sub>` or `tool:<op>`.
pub async fn put_value(
    conn: &mut PgConnection,
    conversation: &str,
    key: &str,
    value: &str,
    produced_by: &str,
    ttl_s: Option<i64>,
) -> ApiResult<()> {
    check_key(key)?;
    check_value(value)?;
    upsert(
        conn,
        conversation,
        key,
        "value",
        Some(value),
        None,
        produced_by,
        expiry(ttl_s),
    )
    .await
}

/// Set a `secret` for `key` of `conversation`, sealed under `keyring` (AAD = `tenant|conversation|key`).
pub async fn put_secret(
    conn: &mut PgConnection,
    keyring: &Keyring,
    conversation: &str,
    key: &str,
    secret: &str,
    produced_by: &str,
    ttl_s: Option<i64>,
) -> ApiResult<()> {
    check_key(key)?;
    check_value(secret)?;
    let tenant = tenant_text(conn).await?;
    let sealed = keyring
        .seal(&aad(&tenant, conversation, key), secret.as_bytes())
        .map_err(|_| unavailable("could not seal a secret"))?;
    upsert(
        conn,
        conversation,
        key,
        "secret",
        None,
        Some(&sealed),
        produced_by,
        expiry(ttl_s),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn upsert(
    conn: &mut PgConnection,
    conversation: &str,
    key: &str,
    kind: &str,
    value: Option<&str>,
    secret: Option<&str>,
    produced_by: &str,
    expires_at: Option<DateTime<Utc>>,
) -> ApiResult<()> {
    sqlx::query(
        "INSERT INTO conversation_state \
            (tenant_id, job_id, conversation_id, key, kind, value, secret, produced_by, expires_at, updated_at) \
         VALUES (app_current_tenant(), app_current_job(), $1, $2, $3, $4, $5, $6, $7, now()) \
         ON CONFLICT (tenant_id, job_id, conversation_id, key) DO UPDATE \
            SET kind = EXCLUDED.kind, value = EXCLUDED.value, secret = EXCLUDED.secret, \
                produced_by = EXCLUDED.produced_by, expires_at = EXCLUDED.expires_at, updated_at = now()",
    )
    .bind(conversation)
    .bind(key)
    .bind(kind)
    .bind(value)
    .bind(secret)
    .bind(produced_by)
    .bind(expires_at)
    .execute(conn)
    .await?;
    Ok(())
}

/// The value of `key` (a secret opened): runtime use only, never for a model message, log or audit. `None` when
/// absent or expired; an undecryptable secret is an error that carries no value.
pub async fn get(
    conn: &mut PgConnection,
    keyring: &Keyring,
    conversation: &str,
    key: &str,
) -> ApiResult<Option<String>> {
    let row = sqlx::query(
        "SELECT kind, value, secret, expires_at FROM conversation_state \
          WHERE conversation_id = $1 AND key = $2",
    )
    .bind(conversation)
    .bind(key)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(r) = row else { return Ok(None) };
    if !is_live(r.get("expires_at"), Utc::now()) {
        return Ok(None);
    }
    if r.get::<String, _>("kind") == "value" {
        return Ok(r.get("value"));
    }
    let sealed: String = r.get("secret");
    let tenant = tenant_text(conn).await?;
    let bytes = keyring
        .open(&aad(&tenant, conversation, key), &sealed)
        .map_err(|_| unavailable("a secret does not open"))?;
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| unavailable("a secret is not text"))
}

/// What the model may see: for each of `keys`, whether it is present (and live).
pub async fn present(
    conn: &mut PgConnection,
    conversation: &str,
    keys: &[&str],
) -> ApiResult<BTreeMap<String, bool>> {
    let live = load(conn, conversation).await?;
    Ok(keys
        .iter()
        .map(|k| ((*k).to_owned(), live.contains_key(*k)))
        .collect())
}

/// Every live entry of `conversation`: plain values as-is, secrets as [`StateValue::Secret`] (not opened).
pub async fn load(
    conn: &mut PgConnection,
    conversation: &str,
) -> ApiResult<BTreeMap<String, StateValue>> {
    let rows = sqlx::query(
        "SELECT key, kind, value, expires_at FROM conversation_state WHERE conversation_id = $1",
    )
    .bind(conversation)
    .fetch_all(conn)
    .await?;
    let now = Utc::now();
    Ok(rows
        .into_iter()
        .filter(|r| is_live(r.get("expires_at"), now))
        .map(|r| {
            let v = match r.get::<String, _>("kind").as_str() {
                "value" => {
                    StateValue::Value(r.get::<Option<String>, _>("value").unwrap_or_default())
                }
                _ => StateValue::Secret,
            };
            (r.get("key"), v)
        })
        .collect())
}

/// Remove `key` of `conversation` (a no-op when absent).
pub async fn forget(conn: &mut PgConnection, conversation: &str, key: &str) -> ApiResult<()> {
    sqlx::query("DELETE FROM conversation_state WHERE conversation_id = $1 AND key = $2")
        .bind(conversation)
        .bind(key)
        .execute(conn)
        .await?;
    Ok(())
}

/// The carry-over key of a conversation: `subject:<id>`, else `channel:<id>`; `None` for an unidentified one.
pub fn subject_key_of(state: &BTreeMap<String, StateValue>) -> Option<String> {
    let pick = |k: &str| {
        state
            .get(k)
            .and_then(StateValue::as_value)
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
    };
    pick(SUBJECT)
        .map(|s| format!("subject:{s}"))
        .or_else(|| pick(CHANNEL_SUBJECT).map(|s| format!("channel:{s}")))
}

/// Keep `key` of `conversation` against its subject until its own expiry (the key is declared
/// `persist_for_subject`). A secret is re-sealed for the subject scope (AAD = `tenant|subject_key|key`). Returns
/// whether anything was kept (false: no such live key, or the conversation has no subject).
pub async fn remember_for_subject(
    conn: &mut PgConnection,
    keyring: Option<&Keyring>,
    conversation: &str,
    key: &str,
) -> ApiResult<bool> {
    let state = load(conn, conversation).await?;
    let (Some(sk), Some(entry)) = (subject_key_of(&state), state.get(key)) else {
        return Ok(false);
    };
    let row = sqlx::query(
        "SELECT produced_by, expires_at FROM conversation_state WHERE conversation_id = $1 AND key = $2",
    )
    .bind(conversation)
    .bind(key)
    .fetch_one(&mut *conn)
    .await?;
    let (produced_by, expires_at): (String, Option<DateTime<Utc>>) =
        (row.get("produced_by"), row.get("expires_at"));
    let (kind, value, sealed) = match entry {
        StateValue::Value(v) => ("value", Some(v.clone()), None),
        StateValue::Secret => {
            let Some(kr) = keyring else { return Ok(false) };
            let Some(plain) = get(conn, kr, conversation, key).await? else {
                return Ok(false);
            };
            let tenant = tenant_text(conn).await?;
            let sealed = kr
                .seal(&aad(&tenant, &sk, key), plain.as_bytes())
                .map_err(|_| unavailable("could not seal a secret"))?;
            ("secret", None, Some(sealed))
        }
    };
    sqlx::query(
        "INSERT INTO subject_state \
            (tenant_id, job_id, subject_key, key, kind, value, secret, produced_by, expires_at, updated_at) \
         VALUES (app_current_tenant(), app_current_job(), $1, $2, $3, $4, $5, $6, $7, now()) \
         ON CONFLICT (tenant_id, job_id, subject_key, key) DO UPDATE \
            SET kind = EXCLUDED.kind, value = EXCLUDED.value, secret = EXCLUDED.secret, \
                produced_by = EXCLUDED.produced_by, expires_at = EXCLUDED.expires_at, updated_at = now()",
    )
    .bind(&sk)
    .bind(key)
    .bind(kind)
    .bind(value)
    .bind(sealed)
    .bind(&produced_by)
    .bind(expires_at)
    .execute(conn)
    .await?;
    Ok(true)
}

/// Copy the live carried-over keys of `conversation`'s subject into it, never overwriting a key it already has.
/// Secrets are re-sealed for this conversation (skipped when `keyring` is `None`). Returns the keys copied.
pub async fn carry_over_into(
    conn: &mut PgConnection,
    keyring: Option<&Keyring>,
    conversation: &str,
) -> ApiResult<Vec<String>> {
    let state = load(conn, conversation).await?;
    let Some(sk) = subject_key_of(&state) else {
        return Ok(Vec::new());
    };
    let rows = sqlx::query(
        "SELECT key, kind, value, secret, produced_by, expires_at FROM subject_state WHERE subject_key = $1",
    )
    .bind(&sk)
    .fetch_all(&mut *conn)
    .await?;
    let now = Utc::now();
    let mut copied = Vec::new();
    for r in rows {
        let key: String = r.get("key");
        if state.contains_key(&key) || !is_live(r.get("expires_at"), now) {
            continue;
        }
        let produced_by: String = r.get("produced_by");
        let expires_at: Option<DateTime<Utc>> = r.get("expires_at");
        if r.get::<String, _>("kind") == "value" {
            let v: String = r.get::<Option<String>, _>("value").unwrap_or_default();
            upsert(
                conn,
                conversation,
                &key,
                "value",
                Some(&v),
                None,
                &produced_by,
                expires_at,
            )
            .await?;
        } else {
            let Some(kr) = keyring else { continue };
            let tenant = tenant_text(conn).await?;
            let sealed: String = r.get("secret");
            let Ok(plain) = kr.open(&aad(&tenant, &sk, &key), &sealed) else {
                continue;
            };
            let resealed = kr
                .seal(&aad(&tenant, conversation, &key), &plain)
                .map_err(|_| unavailable("could not seal a secret"))?;
            upsert(
                conn,
                conversation,
                &key,
                "secret",
                None,
                Some(&resealed),
                &produced_by,
                expires_at,
            )
            .await?;
        }
        copied.push(key);
    }
    Ok(copied)
}

/// Write a known customer's identity as state (assertion, staff pick, staff test): `subject`, optional `email` and
/// `name`, and `identity_source`. One helper so every channel writes the same shape.
pub async fn put_known_identity(
    conn: &mut PgConnection,
    conversation: &str,
    source: &str,
    subject: &str,
    email: Option<&str>,
    name: Option<&str>,
    produced_by: &str,
) -> ApiResult<()> {
    put_value(conn, conversation, SUBJECT, subject, produced_by, None).await?;
    for (k, v) in [(EMAIL, email), (NAME, name)] {
        if let Some(v) = v.map(str::trim).filter(|v| !v.is_empty()) {
            put_value(conn, conversation, k, v, produced_by, None).await?;
        }
    }
    put_value(
        conn,
        conversation,
        IDENTITY_SOURCE,
        source,
        produced_by,
        None,
    )
    .await
}

/// Write a non-identifying channel subject (Telegram's chat id, a web chat session placeholder) as state:
/// `channel_subject` and `identity_source = channel`. Never overrides an identity already known.
pub async fn put_channel_subject(
    conn: &mut PgConnection,
    conversation: &str,
    channel_subject: &str,
    produced_by: &str,
) -> ApiResult<()> {
    let channel_subject = channel_subject.trim();
    if channel_subject.is_empty() || channel_subject.chars().count() > MAX_VALUE {
        return Ok(());
    }
    put_value(
        conn,
        conversation,
        CHANNEL_SUBJECT,
        channel_subject,
        produced_by,
        None,
    )
    .await?;
    sqlx::query(
        "INSERT INTO conversation_state \
            (tenant_id, job_id, conversation_id, key, kind, value, produced_by, updated_at) \
         VALUES (app_current_tenant(), app_current_job(), $1, $2, 'value', $3, $4, now()) \
         ON CONFLICT (tenant_id, job_id, conversation_id, key) DO NOTHING",
    )
    .bind(conversation)
    .bind(IDENTITY_SOURCE)
    .bind(SOURCE_CHANNEL)
    .bind(produced_by)
    .execute(conn)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, StateValue)]) -> BTreeMap<String, StateValue> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect()
    }
    fn v(s: &str) -> StateValue {
        StateValue::Value(s.into())
    }

    #[test]
    fn key_format() {
        for ok in ["a", "auth_token", "customer_id", "x9", &"a".repeat(64)] {
            assert!(valid_key(ok), "{ok}");
        }
        for bad in [
            "",
            "A",
            "1a",
            "_a",
            "a-b",
            "a b",
            "a.b",
            "é",
            &"a".repeat(65),
        ] {
            assert!(!valid_key(bad), "{bad}");
        }
    }

    #[test]
    fn reserved_keys() {
        for k in [
            SUBJECT,
            EMAIL,
            NAME,
            CHANNEL_SUBJECT,
            CALLER,
            CALLER_LABEL,
            IDENTITY_SOURCE,
            IDENTITY_STRENGTH,
            FR_PROJECT_ID,
            FR_PROJECT_TASK,
            CONVEYOR_PROJECT_ID,
            CONVEYOR_PROJECT_TASK,
        ] {
            assert!(is_reserved(k) && valid_key(k));
        }
        assert!(!is_reserved("auth_token"));
    }

    #[test]
    fn known_subject_needs_subject_and_a_known_source() {
        let s = map(&[(SUBJECT, v("c-1")), (IDENTITY_SOURCE, v("assertion"))]);
        assert_eq!(known_subject(&s), Some("c-1"));
        for src in ["staff_pick", "staff_test"] {
            let s = map(&[(SUBJECT, v(" c-2 ")), (IDENTITY_SOURCE, v(src))]);
            assert_eq!(known_subject(&s), Some("c-2"));
        }
        for src in ["channel", "api_key", "other"] {
            let s = map(&[(SUBJECT, v("c-1")), (IDENTITY_SOURCE, v(src))]);
            assert_eq!(known_subject(&s), None, "{src}");
        }
        assert_eq!(known_subject(&map(&[(SUBJECT, v("c-1"))])), None);
        assert_eq!(
            known_subject(&map(&[(IDENTITY_SOURCE, v("assertion"))])),
            None
        );
        assert_eq!(
            known_subject(&map(&[
                (SUBJECT, v("  ")),
                (IDENTITY_SOURCE, v("assertion"))
            ])),
            None
        );
        // a sealed subject is never a customer id
        let s = map(&[
            (SUBJECT, StateValue::Secret),
            (IDENTITY_SOURCE, v("assertion")),
        ]);
        assert_eq!(known_subject(&s), None);
    }

    /// Owner 2026-11-01: a tenant IAM procedure's strength decides the level. `identified` is a known person at
    /// level 1 but NOT a verified customer: `known_subject` stays false, so the verified-only reads do not run.
    #[test]
    fn an_iam_procedure_is_known_at_its_declared_strength() {
        let s = map(&[
            (SUBJECT, v("c-1")),
            (IDENTITY_SOURCE, v(SOURCE_IAM)),
            (IDENTITY_STRENGTH, v(STRENGTH_VERIFIED)),
        ]);
        assert_eq!(known_subject(&s), Some("c-1"));
        assert_eq!(identified_subject(&s), Some("c-1"));
        let s = map(&[
            (SUBJECT, v("c-1")),
            (IDENTITY_SOURCE, v(SOURCE_IAM)),
            (IDENTITY_STRENGTH, v(STRENGTH_IDENTIFIED)),
        ]);
        assert_eq!(known_subject(&s), None, "identified is not verified");
        assert_eq!(identified_subject(&s), Some("c-1"));
        // iam with no strength (a row written before 0248) is identified, never verified
        let s = map(&[(SUBJECT, v("c-1")), (IDENTITY_SOURCE, v(SOURCE_IAM))]);
        assert_eq!(known_subject(&s), None);
        assert_eq!(identified_subject(&s), None);
        // a channel's subject is not a person at any level
        let s = map(&[
            (SUBJECT, v("c-1")),
            (IDENTITY_SOURCE, v(SOURCE_CHANNEL)),
            (IDENTITY_STRENGTH, v(STRENGTH_VERIFIED)),
        ]);
        assert_eq!(identified_subject(&s), None);
    }

    #[test]
    fn expiry_filtering() {
        let now = Utc::now();
        let d = chrono::Duration::seconds(1);
        assert!(is_live(None, now));
        assert!(is_live(Some(now + d), now));
        assert!(!is_live(Some(now), now));
        assert!(!is_live(Some(now - d), now));
    }

    #[test]
    fn subject_key_prefers_subject() {
        assert_eq!(subject_key_of(&BTreeMap::new()), None);
        let s = map(&[(CHANNEL_SUBJECT, v("tg-9"))]);
        assert_eq!(subject_key_of(&s).as_deref(), Some("channel:tg-9"));
        let s = map(&[(CHANNEL_SUBJECT, v("tg-9")), (SUBJECT, v("c-1"))]);
        assert_eq!(subject_key_of(&s).as_deref(), Some("subject:c-1"));
    }

    #[test]
    fn debug_hides_values() {
        assert_eq!(format!("{:?}", v("secret-ish")), "Value(..)");
        assert_eq!(format!("{:?}", StateValue::Secret), "Secret");
    }

    #[test]
    fn aad_binds_tenant_conversation_key() {
        assert_eq!(aad("t", "c", "k"), b"t|c|k".to_vec());
    }

    #[test]
    fn expiry_ttl_is_clamped() {
        assert!(expiry(None).is_none());
        let e = expiry(Some(-5)).unwrap();
        assert!(e > Utc::now() - chrono::Duration::seconds(1));
    }
}
