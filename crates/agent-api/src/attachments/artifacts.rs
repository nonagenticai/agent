//! The artifact_service client of the attachment store (SW-9; owner direction 2026-10-27: attachment bytes live in the
//! platform artifact_service, which scans every upload with ClamAV through storage_service, not in Postgres).
//!
//! - Upload: `POST /projects/{PROJECT_ID}/artifacts/upload?project_id={PROJECT_ID}` (as measured against a stack built
//!   from origin/main; answers 200 with `artifact_id`, any 2xx is read; multipart: `file`, `filename`, `content_type`, `kind`,
//!   `sensitivity`, `tags`), download `GET …/artifacts/{id}/download`, delete `DELETE …/artifacts/{id}` (artifact_service
//!   origin/main `src/routes/artifacts.py`). The project is a bare uuid with no entity behind it: one fixed constant.
//! - Auth: the TENANT's own active licence token as `Authorization: Bearer` plus `X-Tenant-Id: <tenant uuid>`
//!   (artifact_service `src/auth/middleware.py`: an ACTIVE licence in `Authorization` is a licence principal and a plain
//!   `X-Tenant-Id` beside it must equal the licence's tenant, else 403 `TENANT_PROOF_MISMATCH`). It checks no licence app
//!   or audience: any active licence proves its tenant. A bare `X-Tenant-Id` (accepted today under observe mode) is
//!   never sent: a tenant without a readable licence cannot store a file ([`Fail::Storage`]). The token is opened with
//!   the licence cipher per call and never logged. Rows are `X-Visibility: user`, the most private the API offers: only
//!   the licence's subject (stable across a re-issue, crate::licensing::reissue) reads or deletes them, and the tenant's
//!   artifact library does not list them.
//! - Names: the artifact's filename is `<attachment id>.<ext>`, never the customer-visible display name. The
//!   artifact carries the tags `conversation_attachment` and `att:<attachment id>` (so an orphan whose upload answer
//!   was lost can be traced to its attachment: [`ArtifactsClient::find_by_attachment`], used by the sweep of the
//!   upload intents committed before every upload, 0223).
//! - Failures fail closed ([`Fail`]): 422 `malware` → [`Fail::Malware`]; 503 `scan_unavailable` →
//!   [`Fail::ScanUnavailable`]; everything else (any other status, 401/403 included, a timeout, a connection error, an
//!   unreadable answer) → [`Fail::Storage`]. An unknown failure is never an accepted upload.
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use reqwest::multipart::{Form, Part};
use serde_json::Value;
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use super::validate::{Accepted, MAX_BYTES, reason};
use crate::error::ApiResult;
use crate::identity::Identity;
use crate::state::AppState;
use crate::upstream::Auth;

/// The fixed artifact_service project of every conversation attachment:
/// `uuid5(NAMESPACE_URL, "nonagentic:agent:conversation-attachments")`.
pub const PROJECT_ID: Uuid = Uuid::from_u128(0x1ccdade5_c58a_5505_bd5d_20eea03d49eb);
/// `ARTIFACTS_URL` default (artifact_service's in-cluster Service).
pub const DEFAULT_URL: &str = "http://localhost:8556";
/// `ARTIFACTS_TIMEOUT_SECS` default.
pub const DEFAULT_TIMEOUT_SECS: u64 = 30;
/// The upstream name of the metrics and logs.
const NAME: &str = "artifacts";
/// The tag every attachment artifact carries.
const TAG: &str = "conversation_attachment";

/// `ARTIFACTS_URL` and `ARTIFACTS_TIMEOUT_SECS`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtifactsConfig {
    /// No trailing slash.
    pub url: String,
    /// One call (upload, download or delete).
    pub timeout: Duration,
}

impl Default for ArtifactsConfig {
    fn default() -> Self {
        Self {
            url: DEFAULT_URL.to_string(),
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
        }
    }
}

impl ArtifactsConfig {
    /// Read the two variables (`trimmed`: blank = unset).
    pub fn from_lookup(trimmed: &impl Fn(&str) -> Option<String>) -> Result<Self> {
        let url = match trimmed("ARTIFACTS_URL") {
            Some(v) => crate::config::base_url("ARTIFACTS_URL", v)?,
            None => DEFAULT_URL.to_string(),
        };
        let secs: u64 =
            crate::config::number(trimmed, "ARTIFACTS_TIMEOUT_SECS", DEFAULT_TIMEOUT_SECS)?;
        if !(1..=300).contains(&secs) {
            anyhow::bail!("ARTIFACTS_TIMEOUT_SECS must be between 1 and 300");
        }
        Ok(Self {
            url,
            timeout: Duration::from_secs(secs),
        })
    }
}

/// Why the bytes could not be stored or fetched. Each maps to a refusal reason ([`Fail::reason`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fail {
    /// The scanner convicted the file.
    Malware,
    /// The scanner could not run (fail closed).
    ScanUnavailable,
    /// Anything else: no licence, auth refused, the service down, slow or answering something unreadable.
    Storage,
}

impl Fail {
    /// The refusal reason (metric label and API `reason`).
    pub fn reason(self) -> &'static str {
        match self {
            Fail::Malware => reason::MALWARE,
            Fail::ScanUnavailable => reason::SCAN_UNAVAILABLE,
            Fail::Storage => reason::STORAGE_UNAVAILABLE,
        }
    }
}

/// A tenant's licence as the credential of one call. No `Debug`: the token is a secret.
pub struct Licence {
    pub tenant: Uuid,
    token: String,
}

impl Licence {
    fn auth(&self) -> Auth<'_> {
        Auth {
            token: Some(&self.token),
            tenant: Some(self.tenant),
            actor: None,
            app: None,
        }
    }
}

/// The artifact_service client.
#[derive(Clone)]
pub struct ArtifactsClient {
    http: reqwest::Client,
    cfg: ArtifactsConfig,
    identity: Arc<Identity>,
    db: PgPool,
}

impl std::fmt::Debug for ArtifactsClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArtifactsClient")
            .field("url", &self.cfg.url)
            .finish_non_exhaustive()
    }
}

/// `error.code`, `detail.code` or `code` of an artifact_service answer.
fn body_code(v: &Value) -> Option<&str> {
    v.get("code")
        .and_then(Value::as_str)
        .or_else(|| v.pointer("/error/code").and_then(Value::as_str))
        .or_else(|| v.pointer("/detail/code").and_then(Value::as_str))
}

/// The `kind` form field and file extension of `media_type`.
fn kind_and_ext(media_type: &str) -> (&'static str, &'static str) {
    match media_type {
        "application/pdf" => ("doc", "pdf"),
        "image/png" => ("image", "png"),
        "image/webp" => ("image", "webp"),
        _ => ("image", "jpg"),
    }
}

impl ArtifactsClient {
    pub fn for_state(st: &AppState) -> Self {
        Self {
            http: st.http.clone(),
            cfg: st.cfg.artifacts.clone(),
            identity: st.identity.clone(),
            db: st.db.clone(),
        }
    }

    pub fn db(&self) -> &PgPool {
        &self.db
    }

    fn open(&self, tenant: Uuid, sealed: Option<String>) -> Option<Licence> {
        let sealed = sealed?;
        match self.identity.cipher.open(tenant, &sealed) {
            Ok(token) => Some(Licence { tenant, token }),
            Err(e) => {
                tracing::warn!(%tenant, error = %e, "attachment_licence_unreadable");
                None
            }
        }
    }

    /// The licence of the tenant `conn`'s transaction is scoped to (`app.current_tenant`): `None` when the tenant has
    /// none or it does not open.
    pub async fn licence(&self, conn: &mut PgConnection) -> ApiResult<Option<Licence>> {
        let row: Option<(Uuid, Option<String>)> = sqlx::query_as(
            "SELECT id, licence_token_encrypted FROM tenants WHERE id = app_current_tenant()",
        )
        .fetch_optional(conn)
        .await?;
        Ok(row.and_then(|(tenant, sealed)| self.open(tenant, sealed)))
    }

    /// The licence of `tenant` in its own short transaction (the tombstone worker has no request).
    pub async fn licence_of(&self, tenant: Uuid) -> ApiResult<Option<Licence>> {
        let mut tx = crate::tenancy::begin_scoped(&self.db, tenant).await?;
        let l = self.licence(&mut tx).await?;
        tx.commit().await?;
        Ok(l)
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.cfg.url)
    }

    /// Upload the validated bytes of attachment `att` and answer the artifact id.
    pub async fn upload(&self, lic: &Licence, att: Uuid, a: &Accepted) -> Result<Uuid, Fail> {
        let (kind, ext) = kind_and_ext(a.media_type);
        let filename = format!("{att}.{ext}");
        let part = Part::bytes(a.bytes.clone())
            .file_name(filename.clone())
            .mime_str(a.media_type)
            .map_err(|_| Fail::Storage)?;
        let form = Form::new()
            .part("file", part)
            .text("filename", filename)
            .text("content_type", a.media_type)
            .text("kind", kind)
            .text("sensitivity", "secret")
            .text("tags", format!("{TAG},att:{att}"));
        let path = format!("/projects/{PROJECT_ID}/artifacts/upload?project_id={PROJECT_ID}");
        let req = lic
            .auth()
            .apply(self.http.post(self.url(&path)))
            .header("x-visibility", "user")
            .timeout(self.cfg.timeout)
            .multipart(form);
        let (status, body) = self.send(req, "upload").await?;
        if status.is_success() {
            return serde_json::from_slice::<Value>(&body)
                .ok()
                .and_then(|v| {
                    v.get("artifact_id")
                        .and_then(Value::as_str)
                        .and_then(|s| Uuid::parse_str(s).ok())
                })
                .ok_or_else(|| {
                    tracing::warn!("attachment_upload_unreadable_answer");
                    Fail::Storage
                });
        }
        let json: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        let code = body_code(&json).unwrap_or("");
        let fail = match (status.as_u16(), code) {
            (422, c) if c.eq_ignore_ascii_case("malware") => Fail::Malware,
            (503, c) if c.eq_ignore_ascii_case("scan_unavailable") => Fail::ScanUnavailable,
            _ => Fail::Storage,
        };
        tracing::warn!(
            status = status.as_u16(),
            reason = fail.reason(),
            "attachment_upload_refused"
        );
        Err(fail)
    }

    /// The bytes of `artifact`; `None` when artifact_service has no such artifact.
    pub async fn download(&self, lic: &Licence, artifact: Uuid) -> Result<Option<Vec<u8>>, Fail> {
        let path = format!("/projects/{PROJECT_ID}/artifacts/{artifact}/download");
        let req = lic
            .auth()
            .apply(self.http.get(self.url(&path)))
            .timeout(self.cfg.timeout);
        let resp = crate::observability::upstream_http(NAME, "download".into(), req.send())
            .await
            .map_err(|e| self.transport_failure(&e))?;
        let status = resp.status();
        if status.as_u16() == 404 {
            return Ok(None);
        }
        if !status.is_success() {
            tracing::warn!(status = status.as_u16(), "attachment_download_refused");
            return Err(Fail::Storage);
        }
        if resp.content_length().is_some_and(|n| n > MAX_BYTES as u64) {
            tracing::warn!("attachment_download_over_cap");
            return Err(Fail::Storage);
        }
        let bytes = resp.bytes().await.map_err(|e| self.transport_failure(&e))?;
        if bytes.len() > MAX_BYTES {
            tracing::warn!("attachment_download_over_cap");
            return Err(Fail::Storage);
        }
        Ok(Some(bytes.to_vec()))
    }

    /// Delete `artifact`. A 404 counts as done: it is gone (or never was).
    pub async fn delete(&self, lic: &Licence, artifact: Uuid) -> Result<(), Fail> {
        let path = format!("/projects/{PROJECT_ID}/artifacts/{artifact}");
        let req = lic
            .auth()
            .apply(self.http.delete(self.url(&path)))
            .timeout(self.cfg.timeout);
        let (status, _) = self.send(req, "delete").await?;
        if status.is_success() || status.as_u16() == 404 {
            return Ok(());
        }
        tracing::warn!(status = status.as_u16(), "attachment_delete_refused");
        Err(Fail::Storage)
    }

    /// Delete `artifact` of `tenant` on the tenant's own licence; any failure (no licence included) is an `Err`.
    pub async fn delete_for(&self, tenant: Uuid, artifact: Uuid) -> Result<(), Fail> {
        let lic = match self.licence_of(tenant).await {
            Ok(Some(l)) => l,
            Ok(None) => {
                tracing::warn!(%tenant, "attachment_delete_without_licence");
                return Err(Fail::Storage);
            }
            Err(_) => return Err(Fail::Storage),
        };
        self.delete(&lic, artifact).await
    }

    /// The artifacts of attachment `att` (tag `att:<att>`), found through the project's list endpoint
    /// (`GET /projects/{PROJECT_ID}/artifacts?tags=att:<att>&limit=100`, artifact_service origin/main
    /// `src/routes/artifacts.py` `list_artifacts`: a JSON array of artifacts, each with its `id`). Normally none or one.
    pub async fn find_by_attachment(&self, lic: &Licence, att: Uuid) -> Result<Vec<Uuid>, Fail> {
        let path = format!("/projects/{PROJECT_ID}/artifacts?tags=att:{att}&limit=100");
        let req = lic
            .auth()
            .apply(self.http.get(self.url(&path)))
            .timeout(self.cfg.timeout);
        let (status, body) = self.send(req, "find").await?;
        if status.as_u16() == 404 {
            return Ok(Vec::new());
        }
        if !status.is_success() {
            tracing::warn!(status = status.as_u16(), "attachment_find_refused");
            return Err(Fail::Storage);
        }
        let Some(items) = serde_json::from_slice::<Value>(&body)
            .ok()
            .and_then(|v| v.as_array().cloned())
        else {
            tracing::warn!("attachment_find_unreadable_answer");
            return Err(Fail::Storage);
        };
        // an item without a readable id is an answer we do not understand: never "none found"
        items
            .iter()
            .map(|i| {
                i.get("id")
                    .and_then(Value::as_str)
                    .and_then(|s| Uuid::parse_str(s).ok())
                    .ok_or(Fail::Storage)
            })
            .collect()
    }

    /// Commit the intent to upload attachment `att` of `tenant` (0223), before any byte leaves. Own transaction on the
    /// pool: it survives whatever happens to the request's transaction.
    pub async fn open_intent(&self, tenant: Uuid, att: Uuid) -> Result<(), sqlx::Error> {
        sqlx::query("SELECT agent_attachment_intent_open($1, $2)")
            .bind(tenant)
            .bind(att)
            .execute(&self.db)
            .await
            .map(|_| ())
    }

    /// Drop the intent of an upload that stored nothing or whose artifact is already deleted or tombstoned. Best
    /// effort: if it fails the sweep resolves the intent (finding nothing).
    pub async fn discard_intent(&self, tenant: Uuid, att: Uuid) {
        if let Err(e) = sqlx::query("SELECT agent_attachment_intent_discard($1, $2)")
            .bind(tenant)
            .bind(att)
            .execute(&self.db)
            .await
        {
            tracing::warn!(%tenant, attachment = %att, error = %e, "attachment_intent_discard_failed");
        }
    }

    /// Give up on the upload of attachment `att` that stored `artifact` of `tenant` and was never kept: delete it
    /// (else queue its tombstone), then drop the intent — the artifact is tracked either way.
    pub async fn abandon_upload(&self, tenant: Uuid, att: Uuid, artifact: Uuid) {
        self.abandon(tenant, artifact).await;
        self.discard_intent(tenant, att).await;
    }

    /// Give up on `artifact` of `tenant` (uploaded, never kept): delete it now, else queue its tombstone so the
    /// worker retries. Best effort: nothing here can fail the caller.
    pub async fn abandon(&self, tenant: Uuid, artifact: Uuid) {
        if self.delete_for(tenant, artifact).await.is_ok() {
            return;
        }
        if let Err(e) = sqlx::query("SELECT agent_attachment_tombstone_queue($1, $2)")
            .bind(tenant)
            .bind(artifact)
            .execute(&self.db)
            .await
        {
            tracing::error!(%tenant, %artifact, error = %e, "attachment_tombstone_queue_failed");
        }
    }

    async fn send(
        &self,
        req: reqwest::RequestBuilder,
        op: &'static str,
    ) -> Result<(reqwest::StatusCode, Vec<u8>), Fail> {
        let resp = crate::observability::upstream_http(NAME, op.into(), req.send())
            .await
            .map_err(|e| self.transport_failure(&e))?;
        let status = resp.status();
        let body = resp.bytes().await.map_err(|e| self.transport_failure(&e))?;
        Ok((status, body.to_vec()))
    }

    /// A transport failure: logged without the URL (reqwest errors can carry it) and without any token.
    fn transport_failure(&self, e: &reqwest::Error) -> Fail {
        tracing::warn!(
            upstream = NAME,
            timeout = e.is_timeout(),
            connect = e.is_connect(),
            "upstream unreachable"
        );
        Fail::Storage
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn project_id_is_the_uuid5_of_its_name() {
        // python3 -c "import uuid; print(uuid.uuid5(uuid.NAMESPACE_URL, 'nonagentic:agent:conversation-attachments'))"
        assert_eq!(
            PROJECT_ID.to_string(),
            "1ccdade5-c58a-5505-bd5d-20eea03d49eb"
        );
    }

    #[test]
    fn body_code_reads_the_three_envelopes() {
        assert_eq!(body_code(&json!({"code": "malware"})), Some("malware"));
        assert_eq!(
            body_code(&json!({"error": {"code": "malware"}})),
            Some("malware")
        );
        assert_eq!(
            body_code(&json!({"detail": {"code": "scan_unavailable"}})),
            Some("scan_unavailable")
        );
        assert_eq!(body_code(&json!({"detail": "x"})), None);
    }

    #[test]
    fn config_defaults_and_bounds() {
        let none = |_: &str| None::<String>;
        assert_eq!(
            ArtifactsConfig::from_lookup(&none).unwrap(),
            ArtifactsConfig::default()
        );
        let bad = |k: &str| (k == "ARTIFACTS_TIMEOUT_SECS").then(|| "0".to_string());
        assert!(ArtifactsConfig::from_lookup(&bad).is_err());
        let url = |k: &str| (k == "ARTIFACTS_URL").then(|| "http://x:1/".to_string());
        assert_eq!(
            ArtifactsConfig::from_lookup(&url).unwrap().url,
            "http://x:1"
        );
    }
}
