//! Per-job API keys for machine callers (B13 §4). Not the static service tokens crate::auth dropped: a key is minted
//! per job by a person holding `integrations_manager`, stored only as its sha256, revocable, optionally expiring, and
//! every create / revoke is audited (`job_api_key.create`, `job_api_key.revoke`).
//!
//! - `GET /api/jobs/{job}/api-keys` — `{items: [key], next: null}`, newest first.
//! - `POST /api/jobs/{job}/api-keys {label, expires_at?}` — 201 `{key, secret: "ak_…"}`; the secret is shown this once.
//!   422 `label_required` (1 to 120 characters), 422 `invalid_expires_at` (RFC 3339, in the future).
//! - `DELETE /api/jobs/{job}/api-keys/{id}` — 204; revoking a revoked key is 204 again (audited once); 404
//!   `api_key_not_found`.
//!
//! A key view: `{id, label, prefix, state: "active"|"expired"|"revoked", created_by, created_at, last_used_at,
//! expires_at, revoked_at}` (instants RFC 3339 with milliseconds, or null).
//!
//! [`JobCaller`] is the guard of the interaction routes (crate::interaction::routes, email, MCP, crate::voice): a
//! member of the job's tenant exactly as `JobMember`, or an API key presented as `Authorization: Bearer ak_…` or
//! `X-Api-Key: ak_…`. The key's job must be the path's job (else 404 `job_not_found`); an unknown, malformed,
//! revoked or expired key is 401 `invalid_api_key` (one answer for all four). A key reaches no other route: every
//! other guard verifies a Keycloak token, which an `ak_` secret is not (401 `unauthenticated`). A key caller is party
//! `api:<key id>` (role `system`, a machine caller) unless the request names its own party. A key passes licence gate 2 exactly as a member does
//! (crate::licensing::gate::check_request, after the tenant status checks; `LICENSE_GATE_2_ENABLED`).
use axum::Router;
use axum::extract::{FromRequestParts, RawPathParams, State};
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::response::Json;
use axum::routing::{delete, get};
use chrono::{DateTime, Utc};
use ring::rand::{SecureRandom, SystemRandom};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::Row;
use uuid::Uuid;

use super::model::{InteractionRequest, Party, Scope};
use crate::audit::{self, Event};
use crate::auth::TenantStatus;
use crate::conversations::iso;
use crate::error::{ApiError, ApiJson, ApiPath, ApiResult};
use crate::state::AppState;
use crate::tenancy::{JobMember, JobRequires, begin_scoped, role};

/// Every key starts with this.
pub const KEY_PREFIX: &str = "ak_";
/// Random bytes in a key (200 bits = 40 base32 characters).
pub const KEY_BYTES: usize = 25;
/// The characters of a key shown in lists (`ak_` + 7).
pub const SHOWN_PREFIX: usize = 10;
/// The header a key may come in instead of `Authorization`.
pub const API_KEY_HEADER: &str = "x-api-key";
/// `last_used_at` is written at most this often per key (seconds; the literal in [`by_key`]'s UPDATE).
pub const LAST_USED_EVERY_S: i64 = 60;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/jobs/{job}/api-keys",
            get(job_api_keys).post(create_job_api_key),
        )
        .route("/api/jobs/{job}/api-keys/{id}", delete(revoke_job_api_key))
}

const B32: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";

/// A fresh secret: `ak_` + 40 lowercase base32 characters (25 random bytes).
pub fn new_secret() -> ApiResult<String> {
    let mut b = [0u8; KEY_BYTES];
    SystemRandom::new()
        .fill(&mut b)
        .map_err(|_| ApiError::Internal(anyhow::anyhow!("no randomness for an API key")))?;
    let mut bits: u64 = 0;
    let mut n = 0;
    let mut out = String::from(KEY_PREFIX);
    for byte in b {
        bits = (bits << 8) | u64::from(byte);
        n += 8;
        while n >= 5 {
            n -= 5;
            out.push(B32[((bits >> n) & 31) as usize] as char);
        }
    }
    Ok(out)
}

/// The shape of a secret: `ak_` + exactly 40 lowercase base32 characters.
pub fn well_formed(secret: &str) -> bool {
    secret.strip_prefix(KEY_PREFIX).is_some_and(|rest| {
        rest.len() == KEY_BYTES * 8 / 5 && rest.bytes().all(|c| B32.contains(&c))
    })
}

/// What is stored and looked up: sha256 of the secret.
pub fn hash(secret: &str) -> Vec<u8> {
    Sha256::digest(secret.as_bytes()).to_vec()
}

fn invalid_api_key() -> ApiError {
    ApiError::detailed(
        StatusCode::UNAUTHORIZED,
        "invalid_api_key",
        "the API key is unknown, revoked or expired",
    )
}

fn job_not_found(key: &str) -> ApiError {
    ApiError::Missing("job_not_found", format!("no job {key:?}"))
}

/// The key a request presents, if any: `X-Api-Key` (any value: a non-key there is refused, not ignored), else an
/// `Authorization: Bearer ak_…`. A Bearer that is not an `ak_` secret is a Keycloak token (`None`).
fn presented(parts: &Parts) -> Option<String> {
    if let Some(v) = parts.headers.get(API_KEY_HEADER) {
        return Some(v.to_str().unwrap_or_default().trim().to_string());
    }
    parts
        .headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        // the scheme is case-insensitive (RFC 9110 §11.1): "bearer ak_…" is a key too
        .and_then(|h| {
            h.get(..7)
                .filter(|s| s.eq_ignore_ascii_case("bearer "))
                .map(|_| &h[7..])
        })
        .map(str::trim)
        .filter(|t| t.starts_with(KEY_PREFIX))
        .map(str::to_string)
}

/// Who calls an interaction route of the `{job}` of the path: a member (Keycloak, as `JobMember`) or one of the
/// job's API keys (`key`).
#[derive(Clone, Debug)]
pub struct JobCaller {
    pub tenant: Uuid,
    pub job_id: Uuid,
    pub job_key: String,
    /// The API key that authenticated the request; `None` for a member.
    pub key: Option<Uuid>,
}

impl JobCaller {
    /// The party a key caller speaks as: a `system` (machine) caller `api:<key id>`; `None` for a member. The runtime
    /// turn records the key and its label as the conversation's `caller` state.
    pub fn party(&self) -> Option<Party> {
        self.key.map(|k| Party::system(format!("api:{k}")))
    }

    /// A key caller's request without a party of its own gets [`JobCaller::party`].
    pub fn stamp(&self, req: &mut InteractionRequest) {
        if req.party.is_none() {
            req.party = self.party();
        }
    }
}

impl From<&JobCaller> for Scope {
    fn from(c: &JobCaller) -> Self {
        Self {
            tenant: c.tenant,
            job_id: c.job_id,
            job_key: c.job_key.clone(),
        }
    }
}

impl FromRequestParts<AppState> for JobCaller {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, st: &AppState) -> Result<Self, ApiError> {
        match presented(parts) {
            Some(secret) => by_key(parts, st, &secret).await,
            None => {
                let JobMember(j) = JobMember::from_request_parts(parts, st).await?;
                Ok(JobCaller {
                    tenant: j.t.tenant,
                    job_id: j.job_id,
                    job_key: j.job_key,
                    key: None,
                })
            }
        }
    }
}

/// The key → its tenant and job (the definer lookup, then the tenant's own rows under RLS).
async fn by_key(parts: &mut Parts, st: &AppState, secret: &str) -> ApiResult<JobCaller> {
    if !well_formed(secret) {
        return Err(invalid_api_key());
    }
    let row = sqlx::query(
        "SELECT id, tenant_id, job_id, revoked_at, expires_at FROM agent_job_api_key($1)",
    )
    .bind(hash(secret))
    .fetch_optional(&st.db)
    .await?
    .ok_or_else(invalid_api_key)?;
    let id: Uuid = row.get("id");
    let tenant: Uuid = row.get("tenant_id");
    let job_id: Uuid = row.get("job_id");
    let revoked: Option<DateTime<Utc>> = row.get("revoked_at");
    let expires: Option<DateTime<Utc>> = row.get("expires_at");
    if revoked.is_some() || expires.is_some_and(|e| e <= Utc::now()) {
        tracing::info!(key = %id, revoked = revoked.is_some(), "api_key_refused");
        return Err(invalid_api_key());
    }
    let params = RawPathParams::from_request_parts(parts, st)
        .await
        .map_err(|_| ApiError::BadRequest("invalid_path", "The path is not valid.".into()))?;
    let path_key = params
        .iter()
        .find(|(k, _)| *k == "job")
        .map(|(_, v)| v.to_string())
        .ok_or_else(|| {
            ApiError::Internal(anyhow::anyhow!("job guard on a route without {{job}}"))
        })?;
    let mut tx = begin_scoped(&st.db, tenant).await?;
    let job_key: Option<String> = sqlx::query_scalar("SELECT key FROM jobs WHERE id = $1")
        .bind(job_id)
        .fetch_optional(&mut *tx)
        .await?;
    let status: Option<String> = sqlx::query_scalar("SELECT status FROM tenants WHERE id = $1")
        .bind(tenant)
        .fetch_optional(&mut *tx)
        .await?;
    tx.commit().await?;
    // a key of another job (or tenant) is told nothing more than a member asking for a job it cannot see
    let job_key = job_key
        .filter(|k| *k == path_key)
        .ok_or_else(|| job_not_found(&path_key))?;
    match status.as_deref().and_then(TenantStatus::parse) {
        Some(TenantStatus::Active) => {}
        Some(TenantStatus::Suspended) => {
            return Err(ApiError::Forbidden(
                "tenant_suspended",
                "this tenant is suspended".into(),
            ));
        }
        Some(TenantStatus::Archived) | None => {
            return Err(ApiError::Forbidden(
                "tenant_archived",
                "this tenant is archived".into(),
            ));
        }
    }
    // licence gate 2, after the status checks, with the codes and the flag of a member's `TenantCtx`
    if !crate::licensing::gate::exempt(parts.uri.path()) {
        crate::licensing::gate::check_request(st, tenant).await?;
    }
    let mut tx = begin_scoped(&st.db, tenant).await?;
    sqlx::query(
        "UPDATE job_api_keys SET last_used_at = now() \
          WHERE id = $1 AND (last_used_at IS NULL OR last_used_at < now() - interval '60 seconds')",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    tracing::Span::current().record("tenant", tracing::field::display(tenant));
    Ok(JobCaller {
        tenant,
        job_id,
        job_key,
        key: Some(id),
    })
}

const COLS: &str = "SELECT id, label, prefix, created_by, created_at, last_used_at, expires_at, revoked_at \
     FROM job_api_keys";

fn view(r: &sqlx::postgres::PgRow) -> Value {
    let expires: Option<DateTime<Utc>> = r.get("expires_at");
    let revoked: Option<DateTime<Utc>> = r.get("revoked_at");
    let state = if revoked.is_some() {
        "revoked"
    } else if expires.is_some_and(|e| e <= Utc::now()) {
        "expired"
    } else {
        "active"
    };
    json!({
        "id": r.get::<Uuid, _>("id"),
        "label": r.get::<String, _>("label"),
        "prefix": r.get::<String, _>("prefix"),
        "state": state,
        "created_by": r.get::<String, _>("created_by"),
        "created_at": iso(r.get("created_at")),
        "last_used_at": r.get::<Option<DateTime<Utc>>, _>("last_used_at").map(iso),
        "expires_at": expires.map(iso),
        "revoked_at": revoked.map(iso),
    })
}

fn key_not_found() -> ApiError {
    ApiError::Missing("api_key_not_found", "No such API key in this job.".into())
}

async fn job_api_keys(
    JobRequires(j, _): JobRequires<role::IntegrationsManager>,
    State(st): State<AppState>,
) -> ApiResult<Json<Value>> {
    let mut tx = j.begin(&st.db).await?;
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "{COLS} WHERE job_id = $1 ORDER BY created_at DESC, id"
    )))
    .bind(j.job_id)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(crate::page::paged(
        rows.iter().map(view).collect(),
        None,
    )))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateBody {
    #[serde(default)]
    label: String,
    #[serde(default)]
    expires_at: Option<String>,
}

async fn create_job_api_key(
    JobRequires(j, _): JobRequires<role::IntegrationsManager>,
    State(st): State<AppState>,
    ApiJson(b): ApiJson<CreateBody>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let label = b.label.trim();
    if label.is_empty() || label.chars().count() > 120 || label.chars().any(char::is_control) {
        return Err(ApiError::Unprocessable(
            "label_required",
            "label is 1 to 120 characters".into(),
        ));
    }
    let expires_at = match b.expires_at.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(s) => {
            let t = DateTime::parse_from_rfc3339(s)
                .map(|t| t.with_timezone(&Utc))
                .ok()
                .filter(|t| *t > Utc::now())
                .ok_or_else(|| {
                    ApiError::Unprocessable(
                        "invalid_expires_at",
                        "expires_at is an RFC 3339 instant in the future".into(),
                    )
                })?;
            Some(t)
        }
    };
    let secret = new_secret()?;
    let prefix: String = secret.chars().take(SHOWN_PREFIX).collect();
    let id = Uuid::new_v4();
    let mut tx = j.begin(&st.db).await?;
    sqlx::query(
        "INSERT INTO job_api_keys (id, tenant_id, job_id, label, prefix, key_hash, created_by, expires_at) \
         VALUES ($1, app_current_tenant(), $2, $3, $4, $5, $6, $7)",
    )
    .bind(id)
    .bind(j.job_id)
    .bind(label)
    .bind(&prefix)
    .bind(hash(&secret))
    .bind(&j.t.caller.sub)
    .bind(expires_at)
    .execute(&mut *tx)
    .await?;
    audit::record(
        &mut tx,
        &j.t.actor(),
        Event::new("job_api_key.create", "job_api_key", id.to_string())
            .job(&j.job_key)
            .detail("prefix", prefix.clone())
            .detail("expires_at", expires_at.map(iso)),
    )
    .await?;
    let r = sqlx::query(sqlx::AssertSqlSafe(format!("{COLS} WHERE id = $1")))
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({"key": view(&r), "secret": secret})),
    ))
}

async fn revoke_job_api_key(
    JobRequires(j, _): JobRequires<role::IntegrationsManager>,
    State(st): State<AppState>,
    ApiPath((_, id)): ApiPath<(String, String)>,
) -> ApiResult<StatusCode> {
    let id = Uuid::parse_str(&id).map_err(|_| key_not_found())?;
    let mut tx = j.begin(&st.db).await?;
    let was: Option<Option<DateTime<Utc>>> = sqlx::query_scalar(
        "SELECT revoked_at FROM job_api_keys WHERE id = $1 AND job_id = $2 FOR UPDATE",
    )
    .bind(id)
    .bind(j.job_id)
    .fetch_optional(&mut *tx)
    .await?;
    // revoking a revoked key changes nothing and records nothing
    if was.ok_or_else(key_not_found)?.is_none() {
        sqlx::query("UPDATE job_api_keys SET revoked_at = now() WHERE id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        audit::record(
            &mut tx,
            &j.t.actor(),
            Event::new("job_api_key.revoke", "job_api_key", id.to_string()).job(&j.job_key),
        )
        .await?;
    }
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_are_ak_and_40_base32_and_hash_to_32_bytes() {
        let a = new_secret().unwrap();
        let b = new_secret().unwrap();
        assert_ne!(a, b);
        assert_eq!(a.len(), 43);
        assert!(well_formed(&a), "{a}");
        assert_eq!(hash(&a).len(), 32);
        let upper = a.to_uppercase();
        for bad in [
            "",
            "ak_",
            "wk_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            &a[..42],
            upper.as_str(),
        ] {
            assert!(!well_formed(bad), "{bad}");
        }
    }
}
