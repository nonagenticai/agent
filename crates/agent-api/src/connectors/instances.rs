//! Database access for tenant connector instances (B7 T9/T10): `connector_instance` with its credential state,
//! `connector_contract_result`, `connector_sample`, and the platform `connector_template` rows an instance pins.
//! Every function takes a connection scoped to the tenant (crate::tenancy::TenantCtx::begin); RLS confines it.
//! The JSON builders answer exactly the shapes of `web/src/lib/api/types/connectors.ts` (`ConnectorInstance`,
//! `ContractResult`). Never a credential value: only `{set, updated_at, fingerprint}`.
use std::cmp::Ordering;

use chrono::{DateTime, Utc};
use serde_json::{Map, Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use super::model::ConnectorTemplate;
use super::overrides::OverrideRow;
use super::tenant_templates;
use super::wire::AuthKind;
use crate::error::{ApiError, ApiResult};

/// One `connector_instance` row joined with its credential state (never the ciphertext).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct InstanceRow {
    pub tenant_id: Uuid,
    pub id: String,
    pub label: String,
    pub template_scope: String,
    pub template_key: String,
    pub template_version: String,
    pub env: String,
    pub base_url: String,
    pub env_ids: Value,
    pub max_rps: Option<f64>,
    pub stub: bool,
    pub platform: bool,
    pub status: String,
    pub last_error: Option<Value>,
    pub created_by: String,
    pub created_at: DateTime<Utc>,
    pub cred_updated_at: Option<DateTime<Utc>>,
    pub cred_fingerprint: Option<String>,
    pub cred_version: Option<i32>,
}

impl InstanceRow {
    /// A credential is stored for this instance.
    pub fn has_credential(&self) -> bool {
        self.cred_fingerprint.is_some()
    }

    /// `env_ids` as a map (the `{{config.*}}` / `{{env.*}}` namespaces and the `probe.<field>` inputs).
    pub fn env_map(&self) -> Map<String, Value> {
        self.env_ids.as_object().cloned().unwrap_or_default()
    }
}

/// One `connector_contract_result` row (without the stored response).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ResultRow {
    pub instance_id: String,
    pub op: String,
    pub side_effect: String,
    pub outcome: String,
    pub code: Option<String>,
    pub note: String,
    pub at: Option<DateTime<Utc>>,
}

/// One `connector_sample` row.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SampleRow {
    pub instance_id: String,
    pub op: String,
    pub fetched_at: DateTime<Utc>,
    pub body: Value,
}

/// A result to store (`at` = `None` for outcomes that made no call: `write_unverified`, `needs_ok`).
#[derive(Debug, Clone)]
pub struct NewResult {
    pub op: String,
    pub env: String,
    pub side_effect: &'static str,
    pub outcome: &'static str,
    pub code: Option<&'static str>,
    pub note: String,
    pub at: Option<DateTime<Utc>>,
    pub ran_by: String,
    pub response: Option<Value>,
}

macro_rules! instance_select {
    ($tail:literal) => {
        concat!(
            "SELECT i.tenant_id, i.id, i.label, i.template_scope, i.template_key, i.template_version, i.env, \
                    i.base_url, i.env_ids, i.max_rps::float8 AS max_rps, i.stub, i.platform, i.status, \
                    i.last_error, i.created_by, i.created_at, c.updated_at AS cred_updated_at, \
                    c.fingerprint::text AS cred_fingerprint, c.credential_version AS cred_version \
               FROM connector_instance i \
               LEFT JOIN connector_credential c ON c.tenant_id = i.tenant_id AND c.instance_id = i.id ",
            $tail
        )
    };
}

/// The instance id rule of 0093 (`^[a-z][a-z0-9_-]{2,40}$`).
pub fn valid_id(id: &str) -> bool {
    let b = id.as_bytes();
    (3..=41).contains(&b.len())
        && b[0].is_ascii_lowercase()
        && b[1..]
            .iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_' || *c == b'-')
}

pub fn instance_not_found(id: &str) -> ApiError {
    ApiError::Missing(
        "instance_not_found",
        format!("This tenant has no system \u{201c}{id}\u{201d}."),
    )
}

pub fn template_not_found(key: &str, version: &str) -> ApiError {
    ApiError::Missing(
        "template_not_found",
        format!("No connector template {key}@{version}."),
    )
}

/// Every instance of the tenant: prod, test, all, then id.
pub async fn all(conn: &mut PgConnection) -> ApiResult<Vec<InstanceRow>> {
    Ok(sqlx::query_as::<_, InstanceRow>(instance_select!(
        "ORDER BY CASE i.env WHEN 'prod' THEN 0 WHEN 'test' THEN 1 ELSE 2 END, i.id"
    ))
    .fetch_all(&mut *conn)
    .await?)
}

/// One instance, or `None` (an id that cannot exist is `None` without a query).
pub async fn one(conn: &mut PgConnection, id: &str) -> ApiResult<Option<InstanceRow>> {
    if !valid_id(id) {
        return Ok(None);
    }
    Ok(
        sqlx::query_as::<_, InstanceRow>(instance_select!("WHERE i.id = $1"))
            .bind(id)
            .fetch_optional(&mut *conn)
            .await?,
    )
}

/// One instance, else 404 `instance_not_found`.
pub async fn require(conn: &mut PgConnection, id: &str) -> ApiResult<InstanceRow> {
    one(conn, id).await?.ok_or_else(|| instance_not_found(id))
}

/// Contract results of one instance (`Some`) or of all (`None`), by op.
pub async fn results(conn: &mut PgConnection, instance: Option<&str>) -> ApiResult<Vec<ResultRow>> {
    Ok(sqlx::query_as::<_, ResultRow>(
        "SELECT instance_id, op, side_effect, outcome, code, note, at FROM connector_contract_result \
          WHERE ($1::text IS NULL OR instance_id = $1) ORDER BY instance_id, op",
    )
    .bind(instance)
    .fetch_all(&mut *conn)
    .await?)
}

/// Samples of one instance (`Some`) or of all (`None`).
pub async fn samples(conn: &mut PgConnection, instance: Option<&str>) -> ApiResult<Vec<SampleRow>> {
    Ok(sqlx::query_as::<_, SampleRow>(
        "SELECT instance_id, op, fetched_at, body FROM connector_sample \
          WHERE ($1::text IS NULL OR instance_id = $1)",
    )
    .bind(instance)
    .fetch_all(&mut *conn)
    .await?)
}

/// The stored (redacted) response of the last contract test of `op`, if any.
pub async fn stored_response(
    conn: &mut PgConnection,
    instance: &str,
    op: &str,
) -> ApiResult<Option<Value>> {
    Ok(sqlx::query_scalar(
        "SELECT response FROM connector_contract_result \
          WHERE instance_id = $1 AND op = $2 AND response IS NOT NULL",
    )
    .bind(instance)
    .bind(op)
    .fetch_optional(&mut *conn)
    .await?)
}

/// Insert or replace the result of `(instance, op)`.
pub async fn upsert_result(
    conn: &mut PgConnection,
    tenant: Uuid,
    instance: &str,
    r: &NewResult,
) -> ApiResult<()> {
    sqlx::query(
        "INSERT INTO connector_contract_result \
                (tenant_id, instance_id, op, env, side_effect, outcome, code, note, at, ran_by, response) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) \
         ON CONFLICT (tenant_id, instance_id, op) DO UPDATE \
            SET env = EXCLUDED.env, side_effect = EXCLUDED.side_effect, outcome = EXCLUDED.outcome, \
                code = EXCLUDED.code, note = EXCLUDED.note, at = EXCLUDED.at, ran_by = EXCLUDED.ran_by, \
                response = EXCLUDED.response",
    )
    .bind(tenant)
    .bind(instance)
    .bind(&r.op)
    .bind(&r.env)
    .bind(r.side_effect)
    .bind(r.outcome)
    .bind(r.code)
    .bind(&r.note)
    .bind(r.at)
    .bind(&r.ran_by)
    .bind(&r.response)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Insert or replace the instance's sample; its `fetched_at`.
pub async fn upsert_sample(
    conn: &mut PgConnection,
    tenant: Uuid,
    instance: &str,
    op: &str,
    body: &Value,
) -> ApiResult<DateTime<Utc>> {
    Ok(sqlx::query_scalar(
        "INSERT INTO connector_sample (tenant_id, instance_id, op, body) VALUES ($1, $2, $3, $4) \
         ON CONFLICT (tenant_id, instance_id) DO UPDATE \
            SET op = EXCLUDED.op, body = EXCLUDED.body, fetched_at = now() \
         RETURNING fetched_at",
    )
    .bind(tenant)
    .bind(instance)
    .bind(op)
    .bind(body)
    .fetch_one(&mut *conn)
    .await?)
}

/// Set the instance status and `last_error` (`{code, message, at}`).
pub async fn set_failed_status(
    conn: &mut PgConnection,
    instance: &str,
    status: &str,
    code: &str,
    message: &str,
) -> ApiResult<()> {
    let err = json!({"code": code, "message": message, "at": Utc::now()});
    sqlx::query(
        "UPDATE connector_instance SET status = $2, last_error = $3, updated_at = now() WHERE id = $1",
    )
    .bind(instance)
    .bind(status)
    .bind(err)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Names of `subs` (`crate::members::names`: display name, else email, else the sub).
pub async fn names(conn: &mut PgConnection, subs: &[String]) -> ApiResult<Map<String, Value>> {
    let found = crate::members::names::lookup(&mut *conn, subs).await?;
    Ok(found
        .into_iter()
        .filter(|(s, _)| !s.is_empty())
        .map(|(s, n)| (s, Value::String(n)))
        .collect())
}

const TEMPLATE_JSON: &str = "\
SELECT to_jsonb(t) || jsonb_build_object('ops', coalesce((
           SELECT jsonb_agg(to_jsonb(o) ORDER BY o.object, o.verb, o.variant)
             FROM connector_template_op o WHERE o.key = t.key AND o.version = t.version), '[]'::jsonb))
  FROM connector_template t
 WHERE ($1::text IS NULL OR t.key = $1) AND ($2::text IS NULL OR t.version = $2)
 ORDER BY t.key, t.version";

fn parse_template(v: Value) -> ApiResult<ConnectorTemplate> {
    serde_json::from_value::<ConnectorTemplate>(v).map_err(|e| {
        ApiError::Internal(anyhow::anyhow!(
            "a connector_template row does not parse: {e}"
        ))
    })
}

/// One platform template with its operations.
pub async fn platform_template(
    conn: &mut PgConnection,
    key: &str,
    version: &str,
) -> ApiResult<Option<ConnectorTemplate>> {
    let row: Option<Value> = sqlx::query_scalar(TEMPLATE_JSON)
        .bind(Some(key))
        .bind(Some(version))
        .fetch_optional(&mut *conn)
        .await?;
    row.map(parse_template).transpose()
}

/// Every platform template with its operations, by key then version.
pub async fn platform_templates(conn: &mut PgConnection) -> ApiResult<Vec<ConnectorTemplate>> {
    let rows: Vec<Value> = sqlx::query_scalar(TEMPLATE_JSON)
        .bind(None::<&str>)
        .bind(None::<&str>)
        .fetch_all(&mut *conn)
        .await?;
    rows.into_iter().map(parse_template).collect()
}

/// `(version, status)` of every version of a platform template key.
pub async fn versions_of(conn: &mut PgConnection, key: &str) -> ApiResult<Vec<(String, String)>> {
    Ok(
        sqlx::query_as("SELECT version, status FROM connector_template WHERE key = $1")
            .bind(key)
            .fetch_all(&mut *conn)
            .await?,
    )
}

/// `key@version` in its scope: the platform template, or (`scope = "tenant"`) the tenant's own template (0092,
/// B7 T11) in the platform shape (crate::connectors::tenant_templates::as_connector_template: side effects from the
/// operation catalogue, the default fault set, the 10 s timeout), so both render, call and map through one path.
pub async fn template_in_scope(
    conn: &mut PgConnection,
    scope: &str,
    key: &str,
    version: &str,
) -> ApiResult<Option<ConnectorTemplate>> {
    if scope == "tenant" {
        return match tenant_templates::one(conn, key, version).await? {
            Some(row) => Ok(Some(tenant_templates::as_connector_template(&row)?)),
            None => Ok(None),
        };
    }
    platform_template(conn, key, version).await
}

/// The template an instance pins (platform or tenant scope), else 404 `template_not_found`.
pub async fn pinned_template(
    conn: &mut PgConnection,
    inst: &InstanceRow,
) -> ApiResult<ConnectorTemplate> {
    template_in_scope(
        conn,
        &inst.template_scope,
        &inst.template_key,
        &inst.template_version,
    )
    .await?
    .ok_or_else(|| template_not_found(&inst.template_key, &inst.template_version))
}

/// TS `localeCompare(…, 'en', {numeric: true})` for version strings: digit runs compare as numbers.
pub fn cmp_version(a: &str, b: &str) -> Ordering {
    fn chunks(s: &str) -> Vec<(bool, String)> {
        let mut out: Vec<(bool, String)> = Vec::new();
        for c in s.chars() {
            let digit = c.is_ascii_digit();
            match out.last_mut() {
                Some((d, run)) if *d == digit => run.push(c),
                _ => out.push((digit, c.to_string())),
            }
        }
        out
    }
    let (ca, cb) = (chunks(a), chunks(b));
    for (x, y) in ca.iter().zip(cb.iter()) {
        let ord = match (x, y) {
            ((true, n), (true, m)) => {
                let n = n.trim_start_matches('0');
                let m = m.trim_start_matches('0');
                n.len().cmp(&m.len()).then_with(|| n.cmp(m))
            }
            ((_, s), (_, t)) => s.to_lowercase().cmp(&t.to_lowercase()),
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    ca.len().cmp(&cb.len())
}

/// `ContractResult` JSON.
pub fn result_json(r: &ResultRow) -> Value {
    json!({
        "op": r.op,
        "side_effect": r.side_effect,
        "outcome": r.outcome,
        "code": r.code,
        "note": r.note,
        "at": r.at,
    })
}

/// `ConnectorInstance` JSON; `results`, `samples` and `overrides` (the working `field_mapping_override` rows, 0094,
/// B7 T12) may hold other instances' rows (filtered here). `auth_kind` is the pinned template's; `credential_required`
/// follows from it (a licence bearer is minted per call and stores nothing).
pub fn instance_json(
    i: &InstanceRow,
    auth_kind: AuthKind,
    results: &[ResultRow],
    samples: &[SampleRow],
    overrides: &[OverrideRow],
) -> Value {
    let contract: Vec<Value> = results
        .iter()
        .filter(|r| r.instance_id == i.id)
        .map(result_json)
        .collect();
    let sample = samples
        .iter()
        .find(|s| s.instance_id == i.id)
        .map(|s| json!({"op": s.op, "fetched_at": s.fetched_at, "body": s.body}));
    let overrides: Vec<Value> = overrides
        .iter()
        .filter(|o| o.instance_id == i.id)
        .map(OverrideRow::json)
        .collect();
    json!({
        "id": i.id,
        "tenant_id": i.tenant_id,
        "label": i.label,
        "template_key": i.template_key,
        "template_version": i.template_version,
        "template_scope": i.template_scope,
        "env": i.env,
        "base_url": i.base_url,
        "env_ids": i.env_ids,
        "max_rps": i.max_rps,
        "stub": i.stub,
        "platform": i.platform,
        "auth_kind": auth_kind,
        "credential_required": auth_kind.needs_stored_credential(),
        "credential": {
            "set": i.has_credential(),
            "updated_at": i.cred_updated_at,
            "fingerprint": i.cred_fingerprint,
        },
        "status": i.status,
        "last_error": i.last_error,
        "contract": contract,
        "overrides": overrides,
        "sample": sample,
        "created_by": i.created_by,
        "created_at": i.created_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_and_versions() {
        assert!(valid_id("shop-prod"));
        assert!(!valid_id("Shop"));
        assert!(!valid_id("ab"));
        assert!(!valid_id("1abc"));
        assert_eq!(cmp_version("10", "9"), Ordering::Greater);
        assert_eq!(cmp_version("2026-10.1", "2026-07.1"), Ordering::Greater);
        assert_eq!(cmp_version("3", "3"), Ordering::Equal);
        assert_eq!(cmp_version("1.2", "1.10"), Ordering::Less);
    }
}
