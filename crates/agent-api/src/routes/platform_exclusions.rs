//! Tenant-wide platform-knowledge exclusions (PK T7; plan PK-10, owner D1: a tenant opts out of base / industry /
//! department knowledge). The routes proxy strata_rag `/api/platform/exclusions` with the tenant's own pair (rag
//! plane, `strata::gate`); strata enforces an exclusion at once (live on save, not a draft). No transaction is held
//! across a strata call.
//!
//! - `GET /api/knowledge/platform-exclusions` (member) → `[{key_prefix, reason, created_at}]`; platform knowledge off
//!   (strata 404) → `[]`. The listing cache's `platform_pack.excluded_by` is synced from the answer.
//! - `PUT /api/knowledge/platform-exclusions` `{key_prefix, reason}` (knowledge_manager) → the stored exclusion
//!   (upsert). 422 `invalid_key_prefix` (not a pack key: 1–6 `/`-separated segments of `[a-z0-9_-]`, at most 512
//!   characters), `invalid_reason` (blank or over 500 characters), `secret_rejected` (the reason looks like a secret
//!   or an email address); 503 `platform_knowledge_off`. Audited `knowledge.platform_exclusion.set` with
//!   `{key_prefix}` only (the reason is free text and stays out of the log).
//! - `DELETE /api/knowledge/platform-exclusions?key_prefix=` (knowledge_manager) → 204; 404 `not_found` (strata: no
//!   exclusion with exactly that prefix). Audited `knowledge.platform_exclusion.remove`.
//!
//! After a write the listing cache is refreshed: `kinds::refresh_packs` (excluded packs leave the listing), then the
//! tenant's exclusions are read back and every cached pack's `excluded_by` is set to the shortest prefix that covers
//! it segment-wise (`industry/retail` covers `industry/retail` and `industry/retail/hr`, not `industry/retailx`), or
//! NULL. The write is already live in strata and audited, so a failed refresh is logged, not answered.
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::audit::{self, Event, scrub};
use crate::error::{ApiError, ApiJson, ApiQuery, ApiResult};
use crate::knowledge::kinds;
use crate::state::AppState;
use crate::strata::rag::{self, Exclusion};
use crate::strata::{self, Plane, Service};
use crate::tenancy::{Member, Requires, TenantCtx, role};

pub fn routes() -> Router<AppState> {
    Router::new().route(
        "/api/knowledge/platform-exclusions",
        get(list).put(set).delete(remove),
    )
}

/// The longest key prefix accepted (T1: pack keys are capped at 512 characters).
const MAX_PREFIX: usize = 512;
/// The longest reason accepted, in characters.
const MAX_REASON: usize = 500;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SetInput {
    key_prefix: String,
    reason: String,
}

#[derive(Deserialize)]
struct RemoveQuery {
    key_prefix: String,
}

fn invalid_key_prefix(prefix: &str) -> ApiError {
    let shown: String = prefix.chars().take(80).collect();
    ApiError::Unprocessable(
        "invalid_key_prefix",
        format!(
            "{shown:?} is not a pack key prefix: 1 to 6 `/`-separated segments of [a-z0-9_-], at most {MAX_PREFIX} characters."
        ),
    )
}

/// `prefix`, trimmed, when it is a pack key (the 0054 / 0126 CHECK), else 422 `invalid_key_prefix`.
fn check_prefix(prefix: &str) -> ApiResult<String> {
    let p = prefix.trim();
    if p.len() > MAX_PREFIX || !kinds::valid_pack_key(p) {
        return Err(invalid_key_prefix(p));
    }
    Ok(p.to_string())
}

/// `reason`, trimmed: 1..=500 characters and nothing secret-shaped (`audit::scrub::check`).
fn check_reason(reason: &str) -> ApiResult<String> {
    let r = reason.trim();
    let n = r.chars().count();
    if n == 0 || n > MAX_REASON {
        return Err(ApiError::Unprocessable(
            "invalid_reason",
            format!("An exclusion needs a reason of 1 to {MAX_REASON} characters."),
        ));
    }
    scrub::check(&json!({ "reason": r })).map_err(|f| {
        ApiError::Unprocessable(
            "secret_rejected",
            format!("{f} looks like a secret or an email address."),
        )
    })?;
    Ok(r.to_string())
}

fn row(x: &Exclusion) -> Value {
    json!({"key_prefix": x.key_prefix, "reason": x.reason, "created_at": x.created_at})
}

/// The rag plane after the gate, in a short scoped transaction (committed before any strata call).
async fn rag_plane<'a>(st: &'a AppState, t: &TenantCtx) -> ApiResult<Plane<'a>> {
    let mut tx = t.begin(&st.db).await?;
    let p = strata::gate(st, &mut tx, t.tenant, Service::Rag)
        .await?
        .with_actor(&t.caller.sub);
    tx.commit().await?;
    Ok(p)
}

/// Every cached pack of the tenant gets `excluded_by` = the shortest of `prefixes` covering its key segment-wise,
/// else NULL (one short transaction; rows already right are not touched).
async fn sync_excluded_by(st: &AppState, t: &TenantCtx, list: &[Exclusion]) -> ApiResult<()> {
    let prefixes: Vec<String> = list
        .iter()
        .map(|x| x.key_prefix.clone())
        .filter(|k| kinds::valid_pack_key(k))
        .collect();
    let mut tx = t.begin(&st.db).await?;
    sqlx::query(
        "WITH m AS ( \
             SELECT pp.pack_key, pp.version, \
                    (SELECT e FROM unnest($2::text[]) AS e \
                      WHERE pp.pack_key = e OR starts_with(pp.pack_key, e || '/') \
                      ORDER BY char_length(e), e LIMIT 1) AS prefix \
               FROM platform_pack pp WHERE pp.tenant_id = $1) \
         UPDATE platform_pack p SET excluded_by = m.prefix \
           FROM m \
          WHERE p.tenant_id = $1 AND p.pack_key = m.pack_key AND p.version = m.version \
            AND p.excluded_by IS DISTINCT FROM m.prefix",
    )
    .bind(t.tenant)
    .bind(prefixes)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// After a write: the listing (`kinds::refresh_packs`) and `excluded_by`. Logged on failure, never answered (the
/// exclusion is already live in strata and audited).
async fn refresh_cache(st: &AppState, t: &TenantCtx, p: &Plane<'_>) {
    if let Err(e) = try_refresh_cache(st, t, p).await {
        tracing::warn!(
            tenant = %t.tenant,
            code = e.code(),
            "platform_exclusion_cache_refresh_failed"
        );
    }
}

async fn try_refresh_cache(st: &AppState, t: &TenantCtx, p: &Plane<'_>) -> ApiResult<()> {
    kinds::refresh_packs(st, t.tenant).await?;
    let list = rag::exclusions(&st.http, p).await?;
    sync_excluded_by(st, t, &list).await
}

async fn record(st: &AppState, t: &TenantCtx, ev: Event) -> ApiResult<()> {
    let mut tx = t.begin(&st.db).await?;
    audit::record(&mut tx, &t.actor(), ev).await?;
    tx.commit().await?;
    Ok(())
}

async fn list(Member(t): Member, State(st): State<AppState>) -> ApiResult<Json<Value>> {
    let p = rag_plane(&st, &t).await?;
    let list = rag::exclusions(&st.http, &p).await?;
    sync_excluded_by(&st, &t, &list).await?;
    Ok(Json(Value::Array(list.iter().map(row).collect())))
}

async fn set(
    Requires(t, _): Requires<role::KnowledgeManager>,
    State(st): State<AppState>,
    ApiJson(input): ApiJson<SetInput>,
) -> ApiResult<Json<Value>> {
    let prefix = check_prefix(&input.key_prefix)?;
    let reason = check_reason(&input.reason)?;
    let p = rag_plane(&st, &t).await?;
    let stored = rag::exclusion_put(&st.http, &p, &prefix, &reason).await?;
    let ev = Event::new("knowledge.platform_exclusion.set", "exclusion", &prefix);
    record(&st, &t, ev.detail("key_prefix", prefix)).await?;
    refresh_cache(&st, &t, &p).await;
    Ok(Json(row(&stored)))
}

async fn remove(
    Requires(t, _): Requires<role::KnowledgeManager>,
    State(st): State<AppState>,
    ApiQuery(q): ApiQuery<RemoveQuery>,
) -> ApiResult<StatusCode> {
    let prefix = check_prefix(&q.key_prefix)?;
    let p = rag_plane(&st, &t).await?;
    rag::exclusion_delete(&st.http, &p, &prefix).await?;
    let ev = Event::new("knowledge.platform_exclusion.remove", "exclusion", &prefix);
    record(&st, &t, ev.detail("key_prefix", prefix)).await?;
    refresh_cache(&st, &t, &p).await;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_is_a_pack_key() {
        assert_eq!(
            check_prefix(" industry/retail ").unwrap(),
            "industry/retail"
        );
        assert_eq!(check_prefix("base").unwrap(), "base");
        for bad in [
            "",
            "Industry",
            "industry/",
            "/x",
            "a/b/c/d/e/f/g",
            "a b",
            "a@1",
        ] {
            assert_eq!(
                check_prefix(bad).unwrap_err().code(),
                "invalid_key_prefix",
                "{bad:?}"
            );
        }
        assert_eq!(
            check_prefix(&"a".repeat(MAX_PREFIX + 1))
                .unwrap_err()
                .code(),
            "invalid_key_prefix"
        );
    }

    #[test]
    fn reason_is_bounded_and_scrubbed() {
        assert_eq!(check_reason("  not for us ").unwrap(), "not for us");
        assert_eq!(check_reason("  ").unwrap_err().code(), "invalid_reason");
        assert_eq!(
            check_reason(&"x".repeat(MAX_REASON))
                .unwrap()
                .chars()
                .count(),
            MAX_REASON
        );
        assert_eq!(
            check_reason(&"x".repeat(MAX_REASON + 1))
                .unwrap_err()
                .code(),
            "invalid_reason"
        );
        assert_eq!(
            check_reason(" jane.doe@example.com ").unwrap_err().code(),
            "secret_rejected"
        );
    }
}
