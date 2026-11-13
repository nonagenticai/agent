//! strata_ingest client (pinned at `ce9590e`: #11 cancel/delete, #12 per-store ocr/vision/transcribe). Shapes mirror
//! strata_ingest `src/api/handlers.rs` and `src/api/store_providers.rs`. The pinned token alone decides the tenant
//! (no `X-Tenant-Id`, no `X-Actor-Id`). Job views are passed on as JSON; [`ui_view`] (T6) maps one to the UI's
//! `IngestJob` (`web/src/lib/api/types/tenant.ts`).
//!
//! `POST /api/stores/{id}/reingest-copy` + `GET …/{job}` (#13 `be0c669`): [`reingest_copy_start`] /
//! [`reingest_copy_get`] (T4a). `POST /api/tenant/providers/test` (#13, T0t): [`providers_test`] (T4c).
use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use super::{Plane, StrataStoreId, query_string};
use crate::error::ApiResult;
use crate::upstream::{INGEST_DELETE_TIMEOUT, LONG_TIMEOUT, Reply};

/// One created job of `POST /api/ingest/jobs`: `status` `queued`, `rejected` (virus scan) or `failed` (a structured
/// source that could not be fetched or parsed).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreatedJob {
    pub job_id: Uuid,
    pub status: String,
}

/// `202 {"jobs": [...]}`; a structured-source body also answers its one job as top-level `job_id`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Created {
    pub jobs: Vec<CreatedJob>,
    #[serde(default)]
    pub job_id: Option<Uuid>,
}

/// `POST /api/ingest/jobs` multipart: `store_id`, optional `source_id` and `options` (JSON) fields first, then one
/// `file` part per job (timeout [`LONG_TIMEOUT`]).
pub async fn create_multipart(
    http: &reqwest::Client,
    p: &Plane<'_>,
    form: reqwest::multipart::Form,
) -> ApiResult<Created> {
    let r = p
        .call("create_multipart", Method::POST, "/api/ingest/jobs")
        .multipart(form)
        .send(http)
        .await?;
    p.decode("create_multipart", r.body)
}

/// `POST /api/ingest/jobs` JSON: `{store_id, url | urls, source_id?, options?}` or a structured `source` body.
pub async fn create_json(http: &reqwest::Client, p: &Plane<'_>, body: Value) -> ApiResult<Created> {
    let r = p
        .call("create_json", Method::POST, "/api/ingest/jobs")
        .json(body)
        .timeout(LONG_TIMEOUT)
        .send(http)
        .await?;
    p.decode("create_json", r.body)
}

/// `GET /api/ingest/jobs` filters (`limit` 1..=200, default 50; `before` = the previous page's `next`).
#[derive(Clone, Debug, Default)]
pub struct ListQuery {
    pub store_id: Option<StrataStoreId>,
    pub status: Option<String>,
    pub limit: Option<i64>,
    pub before: Option<Uuid>,
}

/// `GET /api/ingest/jobs?store_id=&status=&limit=&before=` → `{jobs: [JobView], next: <job id> | null}`.
pub async fn list(http: &reqwest::Client, p: &Plane<'_>, q: &ListQuery) -> ApiResult<Value> {
    let qs = query_string(&[
        ("store_id", q.store_id.map(|s| s.to_string())),
        ("status", q.status.clone()),
        ("limit", q.limit.map(|l| l.to_string())),
        ("before", q.before.map(|b| b.to_string())),
    ]);
    let path = format!("/api/ingest/jobs{qs}");
    let r = p.call("list_jobs", Method::GET, &path).send(http).await?;
    Ok(r.body)
}

/// `GET /api/ingest/jobs/{id}` → the job view (404 for another tenant's job).
pub async fn get(http: &reqwest::Client, p: &Plane<'_>, id: Uuid) -> ApiResult<Value> {
    let path = format!("/api/ingest/jobs/{id}");
    let r = p.call("get_job", Method::GET, &path).send(http).await?;
    Ok(r.body)
}

/// `POST /api/ingest/jobs/{id}/retry` → the view; 409 `conflict` unless the job is `failed`.
pub async fn retry(http: &reqwest::Client, p: &Plane<'_>, id: Uuid) -> ApiResult<Value> {
    let path = format!("/api/ingest/jobs/{id}/retry");
    let r = p.call("retry_job", Method::POST, &path).send(http).await?;
    Ok(r.body)
}

/// `POST /api/ingest/jobs/{id}/cancel` → the view (`cancelled` / `cancelling`); 409 `conflict` once ended.
pub async fn cancel(http: &reqwest::Client, p: &Plane<'_>, id: Uuid) -> ApiResult<Value> {
    let path = format!("/api/ingest/jobs/{id}/cancel");
    let r = p.call("cancel_job", Method::POST, &path).send(http).await?;
    Ok(r.body)
}

/// `DELETE /api/ingest/jobs/{id}` (timeout [`INGEST_DELETE_TIMEOUT`]): 200 with the delete report or 202
/// `{job_id, status: "deleting"}`, so the status is returned. 409 `conflict` while it runs, 409 `child_of_parent_job`
/// with `parent_job_id`, 502 `upstream_unavailable` when a blob cannot be deleted.
pub async fn delete(http: &reqwest::Client, p: &Plane<'_>, id: Uuid) -> ApiResult<Reply> {
    let path = format!("/api/ingest/jobs/{id}");
    p.call("delete_job", Method::DELETE, &path)
        .timeout(INGEST_DELETE_TIMEOUT)
        .send(http)
        .await
}

/// `GET /api/tenant/providers` → `{ocr, vision, transcribe}` (never a key).
pub async fn providers_get(http: &reqwest::Client, p: &Plane<'_>) -> ApiResult<Value> {
    let r = p
        .call("providers_get", Method::GET, "/api/tenant/providers")
        .send(http)
        .await?;
    Ok(r.body)
}

/// `POST /api/tenant/providers/test` (strata_ingest #13 `be0c669`, T0t / 06 §3 A3): the PUT's validation and live probe of one
/// provider (`capability` ocr | vision | transcribe), storing nothing → `{ok, detail}`; a failing provider is `ok: false`,
/// never a 5xx. 422 refusals map through the upstream table (`unknown_capability`, `secret_required_for_new_host`,
/// `provider_host_not_allowed`, the PUT's codes). `Ok(None)` = the route is not deployed (404).
pub async fn providers_test(
    http: &reqwest::Client,
    p: &Plane<'_>,
    body: Value,
) -> ApiResult<Option<Value>> {
    let r = p
        .call("providers_test", Method::POST, "/api/tenant/providers/test")
        .json(body)
        .timeout(LONG_TIMEOUT)
        .send_any(http)
        .await?;
    if r.status == axum::http::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !r.status.is_success() {
        return Err(crate::upstream::refusal(
            p.service().name(),
            r.status,
            &r.body,
        ));
    }
    Ok(Some(r.body))
}

/// `PUT /api/tenant/providers` (whole-set replace, live probe) → the stored view.
pub async fn providers_put(http: &reqwest::Client, p: &Plane<'_>, body: Value) -> ApiResult<Value> {
    let r = p
        .call("providers_put", Method::PUT, "/api/tenant/providers")
        .json(body)
        .timeout(LONG_TIMEOUT)
        .send(http)
        .await?;
    Ok(r.body)
}

/// `GET /api/stores/{id}/providers` → `{store_id, ocr, vision, transcribe}`; 404 `unknown_store`.
pub async fn store_providers_get(
    http: &reqwest::Client,
    p: &Plane<'_>,
    store: StrataStoreId,
) -> ApiResult<Value> {
    let path = format!("/api/stores/{store}/providers");
    let r = p
        .call("store_providers_get", Method::GET, &path)
        .send(http)
        .await?;
    Ok(r.body)
}

/// `PUT /api/stores/{id}/providers` → the view.
pub async fn store_providers_put(
    http: &reqwest::Client,
    p: &Plane<'_>,
    store: StrataStoreId,
    body: Value,
) -> ApiResult<Value> {
    let path = format!("/api/stores/{store}/providers");
    let r = p
        .call("store_providers_put", Method::PUT, &path)
        .json(body)
        .timeout(LONG_TIMEOUT)
        .send(http)
        .await?;
    Ok(r.body)
}

/// `DELETE /api/stores/{id}/providers` (the store inherits again) → the view.
pub async fn store_providers_delete(
    http: &reqwest::Client,
    p: &Plane<'_>,
    store: StrataStoreId,
) -> ApiResult<Value> {
    let path = format!("/api/stores/{store}/providers");
    let r = p
        .call("store_providers_delete", Method::DELETE, &path)
        .send(http)
        .await?;
    Ok(r.body)
}

// ---------------------------------------------------------------------------------------------------------------
// Re-ingest copy (D93, B3 T4a; strata_ingest #13 `be0c669`, `src/reingest.rs`)

/// A document a re-ingest copy could not copy (`originals_not_retained`, `original_too_large`,
/// `original_infected`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissingOriginal {
    pub document_id: Uuid,
    pub reason: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub source_id: Option<String>,
}

/// `ReingestCopyJob`; `state` is `running` | `done` | `failed` (`error` = `{code, message}`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReingestCopyJob {
    pub job: Uuid,
    pub source_store: StrataStoreId,
    pub target_store: StrataStoreId,
    pub state: String,
    #[serde(default)]
    pub documents_done: i64,
    #[serde(default)]
    pub documents_total: i64,
    #[serde(default)]
    pub documents_failed: i64,
    #[serde(default)]
    pub documents_missing: i64,
    #[serde(default)]
    pub missing_originals: Vec<MissingOriginal>,
    #[serde(default)]
    pub error: Option<Value>,
    #[serde(default)]
    pub passes: i64,
    #[serde(default)]
    pub started_at: String,
    #[serde(default)]
    pub finished_at: Option<String>,
}

/// `POST /api/stores/{source}/reingest-copy {target_store}` → 202 with the running copy. 404 `unknown_store`; 409
/// `pack_published`, `target_not_empty`, `conflict`; 422 `invalid_target`.
pub async fn reingest_copy_start(
    http: &reqwest::Client,
    p: &Plane<'_>,
    source: StrataStoreId,
    target: StrataStoreId,
) -> ApiResult<ReingestCopyJob> {
    let path = format!("/api/stores/{source}/reingest-copy");
    let r = p
        .call("reingest_copy_start", Method::POST, &path)
        .json(serde_json::json!({ "target_store": target }))
        .send(http)
        .await?;
    p.decode("reingest_copy_start", r.body)
}

/// `GET /api/stores/{source}/reingest-copy/{job}`.
pub async fn reingest_copy_get(
    http: &reqwest::Client,
    p: &Plane<'_>,
    source: StrataStoreId,
    job: Uuid,
) -> ApiResult<ReingestCopyJob> {
    let path = format!("/api/stores/{source}/reingest-copy/{job}");
    let r = p
        .call("reingest_copy_get", Method::GET, &path)
        .send(http)
        .await?;
    p.decode("reingest_copy_get", r.body)
}

// ---------------------------------------------------------------------------------------------------------------
// The UI's `IngestJob` (B3 T6, plan §5.3)

/// Every strata_ingest job status (`db::jobs::JOB_STATUSES` at `ce9590e`); the UI knows all eight.
pub const JOB_STATUSES: [&str; 8] = [
    "queued",
    "running",
    "cancelling",
    "completed",
    "failed",
    "rejected",
    "cancelled",
    "deleting",
];

/// Statuses of an ended job: its `finished_at` is its last update.
const ENDED: [&str; 4] = ["completed", "failed", "rejected", "cancelled"];

/// The provider slot a step runs on (`db::steps::StepKind`): `ocr_pages` → `ocr`, `caption` → `vision`,
/// `transcribe_segment` → `transcribe`; every other step uses no tenant provider.
pub fn step_slot(kind: &str) -> Option<&'static str> {
    match kind {
        "ocr_pages" => Some("ocr"),
        "caption" => Some("vision"),
        "transcribe_segment" => Some("transcribe"),
        _ => None,
    }
}

fn str_at<'v>(v: &'v Value, k: &str) -> Option<&'v str> {
    v.get(k).and_then(Value::as_str)
}

/// The job's `meta.source_id` (`upload:<id>` for a direct upload, the source's id for a source run).
pub fn source_id(job: &Value) -> Option<&str> {
    job.get("meta").and_then(|m| str_at(m, "source_id"))
}

/// The direct upload a job belongs to (`source_id = upload:<id>`).
pub fn upload_id(job: &Value) -> Option<Uuid> {
    source_id(job)?
        .strip_prefix("upload:")
        .and_then(|u| Uuid::parse_str(u).ok())
}

/// The strata store the job writes to.
pub fn store_id(job: &Value) -> Option<StrataStoreId> {
    str_at(job, "store_id")
        .and_then(|s| Uuid::parse_str(s).ok())
        .map(StrataStoreId)
}

/// A crawl / sitemap page job's parent (`source.parent_job_id`).
pub fn parent_job_id(job: &Value) -> Option<Uuid> {
    job.get("source")
        .and_then(|s| str_at(s, "parent_job_id"))
        .and_then(|p| Uuid::parse_str(p).ok())
}

/// A URL without its query and fragment (a `?api_key=` never reaches a label).
fn bare_url(u: &str) -> String {
    u.split(['?', '#']).next().unwrap_or_default().to_string()
}

/// `(source_kind, source_label)` of the job's `source`. A url job (a website source's page, a crawl page) is a
/// `crawl` to the UI, which has no kind of its own for it.
fn source_of(job: &Value) -> (&'static str, String) {
    static NO_SOURCE: Value = Value::Null;
    let src = job.get("source").unwrap_or(&NO_SOURCE);
    let url = str_at(src, "url")
        .or_else(|| src.get("config").and_then(|c| str_at(c, "url")))
        .map(bare_url)
        .unwrap_or_default();
    match str_at(src, "kind").unwrap_or_default() {
        "upload" => (
            "file",
            str_at(src, "filename").unwrap_or("upload").to_string(),
        ),
        "sitemap" => ("sitemap", url),
        "api" => ("api", url),
        "datatable" => ("datatable", url),
        _ => ("crawl", url),
    }
}

/// `{code, message}` of a strata error object (message scrubbed of `KEY:value` secrets), or `null`.
fn error_of(e: Option<&Value>) -> Value {
    match e {
        Some(e) if e.is_object() => crate::netguard::scrub_secrets(json!({
            "code": str_at(e, "code").unwrap_or("error"),
            "message": str_at(e, "message").unwrap_or_default(),
        })),
        _ => Value::Null,
    }
}

/// One `IngestStep`; strata's `pending` is the UI's `queued`.
fn step_of(s: &Value) -> Value {
    let kind = str_at(s, "kind").unwrap_or_default();
    let status = match str_at(s, "status").unwrap_or("pending") {
        "pending" => "queued",
        other => other,
    };
    json!({
        "kind": kind,
        "slot": step_slot(kind),
        "provider": str_at(s, "provider"),
        "model": str_at(s, "model"),
        "status": status,
        "attempt": s.get("attempt").and_then(Value::as_i64).unwrap_or(0),
        "error": error_of(s.get("error")),
    })
}

/// strata_ingest's job view as the UI's `IngestJob`. `store` is the agent store the job's strata store is bound to
/// (`None`: no agent store holds it); `created_by` is who started it (the upload's member, else the source run).
/// Never carries the job's `source` / `options` objects, so a structured source's settings never reach the UI.
pub fn ui_view(job: &Value, store: Option<Uuid>, created_by: &str) -> Value {
    let status = str_at(job, "status").unwrap_or_default();
    let (kind, label) = source_of(job);
    let finished = if ENDED.contains(&status) {
        job.get("updated_at").cloned()
    } else {
        None
    };
    let pages = job
        .get("children")
        .filter(|c| c.is_object())
        .map(|c| json!({"total": c.get("total"), "by_status": c.get("by_status")}));
    let steps: Vec<Value> = job
        .get("steps")
        .and_then(Value::as_array)
        .map(|a| a.iter().map(step_of).collect())
        .unwrap_or_default();
    json!({
        "id": job.get("id"),
        "store_id": store,
        "source_label": label,
        "source_kind": kind,
        "source_id": source_id(job),
        "status": status,
        "created_at": job.get("created_at"),
        "finished_at": finished,
        "created_by": created_by,
        "documents": job.get("document_ids").and_then(Value::as_array).map_or(0, Vec::len),
        "pages": pages,
        "steps": steps,
        "error": error_of(job.get("error")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ui_view_maps_steps_slots_pages_and_drops_the_source() {
        let job = json!({
            "id": "0192d4c0-0000-7000-8000-000000000001", "store_id": "0192d4c0-0000-7000-8000-000000000002",
            "source": {"kind": "datatable", "url": "https://x.example/t.csv", "auth": {"type": "bearer"},
                       "credentials": {"token": "s3cret"}},
            "options": {"language": "nl"}, "meta": {"source_id": "src-1"}, "status": "failed",
            "error": {"class": "provider", "code": "ocr_failed", "message": "API_TOKEN:s3cret"},
            "document_ids": ["a", "b"], "created_at": "2026-10-21T10:00:00Z", "updated_at": "2026-10-21T10:05:00Z",
            "steps": [
                {"kind": "ocr_pages", "key": "p0", "status": "failed", "attempt": 2, "provider": "mistral",
                 "model": "ocr-1", "error": {"code": "timeout", "message": "slow"}},
                {"kind": "deliver", "key": "", "status": "pending", "attempt": 0},
            ],
            "children": {"total": 3, "by_status": {"completed": 3}},
        });
        let v = ui_view(&job, None, "u-1");
        assert_eq!(v["source_kind"], "datatable");
        assert_eq!(v["source_id"], "src-1");
        assert_eq!(v["finished_at"], "2026-10-21T10:05:00Z");
        assert_eq!(v["documents"], 2);
        assert_eq!(v["pages"]["total"], 3);
        assert_eq!(v["steps"][0]["slot"], "ocr");
        assert_eq!(v["steps"][1]["status"], "queued");
        assert_eq!(v["steps"][1]["slot"], Value::Null);
        assert!(!v.to_string().contains("s3cret"), "{v}");
        assert_eq!(v["error"]["code"], "ocr_failed");
    }
}
