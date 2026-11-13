//! B3 T6: strata_ingest's jobs through agent (plan §5.3: list with the agent store id, the UI view of every status,
//! retry / cancel / delete and agent's own guards `slot_not_configured` and `owned_by_active_run`) and the direct
//! upload into a leaf (plan §5.2 `uploadToStore`: limits, `source_id = upload:<id>`, language, one job per file).
//! Real Postgres 18 (one container for this binary, a fresh database per test) and the in-process strata fakes
//! (`common::strata_fake`).
//!
//! Not here: `retry_datatable_job_starts_new_source_run` needs T8's source runs (`knowledge::runs::start`); T8 adds it
//! with the resubmit branch of `retryIngestJob`.
use crate::common;

use crate::common::strata_fake::{self, StrataFake, lock};
use crate::common::{TestEnv, get, insert_tenant, insert_user, json, person, send};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use uuid::Uuid;

struct W {
    e: TestEnv,
    fake: StrataFake,
    tenant: Uuid,
    /// knowledge_manager
    km: String,
    /// an observer (no role)
    obs: String,
}

async fn member(e: &TestEnv, tenant: Uuid, role: &str) -> String {
    let sub = format!("u-{role}-{}", Uuid::new_v4().simple());
    insert_user(&e.owner, &sub, tenant, role).await;
    person(&sub, &[])
}

async fn world() -> W {
    let fake = strata_fake::start().await;
    let tenant = Uuid::new_v4();
    let e = common::env_with(|v| fake.env(&[tenant], v)).await;
    insert_tenant(&e.owner, tenant, &format!("T6 {tenant}")).await;
    let km = member(&e, tenant, "knowledge_manager").await;
    let obs = member(&e, tenant, "tenant_user").await;
    W {
        e,
        fake,
        tenant,
        km,
        obs,
    }
}

fn code(b: &Value) -> &str {
    b["error"]["code"].as_str().unwrap_or("")
}

/// A `knowledge_upload` row.
#[derive(sqlx::FromRow)]
struct UploadRow {
    id: Uuid,
    language: String,
    files: Value,
    ingest_job_ids: Vec<String>,
    created_by: String,
}

const BOUNDARY: &str = "t6-boundary-5c2a";

/// One form field: (name, file name of a file field, bytes).
type Field<'a> = (&'a str, Option<&'a str>, Vec<u8>);

fn form(fields: &[Field<'_>]) -> Vec<u8> {
    let mut b = Vec::new();
    for (name, file, data) in fields {
        b.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
        match file {
            Some(f) => b.extend_from_slice(
                format!(
                    "Content-Disposition: form-data; name=\"{name}\"; filename=\"{f}\"\r\n\
                     Content-Type: application/octet-stream\r\n\r\n"
                )
                .as_bytes(),
            ),
            None => b.extend_from_slice(
                format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
            ),
        }
        b.extend_from_slice(data);
        b.extend_from_slice(b"\r\n");
    }
    b.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
    b
}

fn file<'a>(name: &'a str, data: &[u8]) -> Field<'a> {
    ("file", Some(name), data.to_vec())
}

impl W {
    async fn call(&self, tok: &str, method: &str, uri: &str) -> (StatusCode, Value) {
        send(&self.e.app, json(method, uri, Some(tok), Value::Null)).await
    }

    async fn read(&self, uri: &str) -> (StatusCode, Value) {
        send(&self.e.app, get(uri, Some(&self.obs))).await
    }

    async fn upload(&self, store: Uuid, fields: &[Field<'_>]) -> (StatusCode, Value) {
        let req = Request::post(format!("/api/knowledge/stores/{store}/upload"))
            .header(
                "content-type",
                format!("multipart/form-data; boundary={BOUNDARY}"),
            )
            .header("authorization", format!("Bearer {}", self.km))
            .body(Body::from(form(fields)))
            .unwrap();
        send(&self.e.app, req).await
    }

    /// A local store inserted as the owner; its id.
    async fn store(&self, key: &str, kind: &str, parent: Option<Uuid>, sid: Option<Uuid>) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO knowledge_store (tenant_id, id, key, label, parent_id, kind, strata_store_id, region, updated_by) \
             VALUES ($1, $2, $3, $3, $4, $5, $6, 'eu', 'test')",
        )
        .bind(self.tenant)
        .bind(id)
        .bind(key)
        .bind(parent)
        .bind(kind)
        .bind(sid)
        .execute(&self.e.owner)
        .await
        .unwrap();
        id
    }

    /// A strata_ingest job of this tenant in the fake; its id.
    fn job(&self, sid: Uuid, status: &str, source: Value, meta: Value, steps: Value) -> Uuid {
        let id = Uuid::new_v4();
        let job = json!({
            "id": id, "tenant": self.tenant, "store_id": sid, "source": source,
            "options": {"ocr": "auto", "captions": true, "language": null}, "meta": meta, "status": status,
            "error": null, "document_ids": [], "created_at": "2026-10-21T10:00:00Z",
            "updated_at": "2026-10-21T10:05:00Z", "steps": steps,
        });
        lock(&self.fake.ingest).jobs.push((self.tenant, job));
        id
    }

    fn upload_job(&self, sid: Uuid, status: &str) -> Uuid {
        self.job(
            sid,
            status,
            json!({"kind": "upload", "filename": "a.pdf"}),
            json!({}),
            json!([]),
        )
    }

    /// The fake's requests `method path` (exact path).
    fn ingest_calls(&self, method: &str, path: &str) -> Vec<strata_fake::Recorded> {
        self.fake
            .ingest_requests()
            .into_iter()
            .filter(|r| r.method == method && r.path == path)
            .collect()
    }

    async fn audit_count(&self, action: &str) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM audit_event WHERE tenant_id = $1 AND action = $2")
            .bind(self.tenant)
            .bind(action)
            .fetch_one(&self.e.owner)
            .await
            .unwrap()
    }

    /// The tenant's oldest upload.
    async fn first_upload(&self) -> UploadRow {
        sqlx::query_as(
            "SELECT id, language, files, ingest_job_ids, created_by FROM knowledge_upload \
              WHERE tenant_id = $1 ORDER BY created_at LIMIT 1",
        )
        .bind(self.tenant)
        .fetch_one(&self.e.owner)
        .await
        .unwrap()
    }

    /// A job of this tenant (the `jobs` table); its id.
    async fn tenant_job(&self, key: &str, languages: &[&str]) -> Uuid {
        sqlx::query_scalar(
            "INSERT INTO jobs (tenant_id, key, label, pins, languages, created_by) \
             VALUES ($1, $2, $2, '[]', $3, 'test') RETURNING id",
        )
        .bind(self.tenant)
        .bind(key)
        .bind(languages)
        .fetch_one(&self.e.owner)
        .await
        .unwrap()
    }
}

// ---------------------------------------------------------------------------------------------------------------
// List and view

#[tokio::test]
async fn list_filters_status_and_agent_store_id() {
    let w = world().await;
    let (sa, sb) = (Uuid::new_v4(), Uuid::new_v4());
    let a = w.store("t6.list.a", "leaf", None, Some(sa)).await;
    w.store("t6.list.b", "leaf", None, Some(sb)).await;
    let unbound = w.store("t6.list.c", "leaf", None, None).await;
    w.upload_job(sa, "completed");
    let failed = w.upload_job(sa, "failed");
    w.upload_job(sb, "failed");

    let (s, b) = w
        .read(&format!(
            "/api/knowledge/ingest-jobs?status=failed&store_id={a}"
        ))
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let items = b["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "{b}");
    assert_eq!(items[0]["id"], json!(failed));
    // the agent store id, never strata's
    assert_eq!(items[0]["store_id"], json!(a));
    let calls = w.ingest_calls("GET", "/api/ingest/jobs");
    let q = &calls.last().unwrap().query;
    assert!(q.contains(&format!("store_id={sa}")), "{q}");
    assert!(q.contains("status=failed"), "{q}");
    assert!(q.contains("limit=100"), "{q}");

    // no filter: every job of the tenant
    let (s, b) = w.read("/api/knowledge/ingest-jobs?status=").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["items"].as_array().unwrap().len(), 3, "{b}");

    // an unknown status: refused here, strata not asked
    let before = w.fake.ingest_requests().len();
    let (s, b) = w.read("/api/knowledge/ingest-jobs?status=done").await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(code(&b), "invalid_status");
    // a store not created in strata holds no job; an unknown store is 404
    let (s, b) = w
        .read(&format!("/api/knowledge/ingest-jobs?store_id={unbound}"))
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["items"], json!([]));
    let (s, b) = w
        .read(&format!(
            "/api/knowledge/ingest-jobs?store_id={}",
            Uuid::new_v4()
        ))
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    assert_eq!(code(&b), "store_not_found");
    assert_eq!(w.fake.ingest_requests().len(), before);
}

#[tokio::test]
async fn get_maps_view_to_ui_shape() {
    let w = world().await;
    let sid = Uuid::new_v4();
    let leaf = w.store("t6.view.leaf", "leaf", None, Some(sid)).await;

    // all eight statuses come through; only an ended job has `finished_at`
    for status in [
        "queued",
        "running",
        "cancelling",
        "completed",
        "failed",
        "rejected",
        "cancelled",
        "deleting",
    ] {
        let id = w.upload_job(sid, status);
        let (s, b) = w.read(&format!("/api/knowledge/ingest-jobs/{id}")).await;
        assert_eq!(s, StatusCode::OK, "{status}: {b}");
        assert_eq!(b["status"], status);
        let ended = matches!(status, "completed" | "failed" | "rejected" | "cancelled");
        assert_eq!(b["finished_at"].is_null(), !ended, "{status}: {b}");
        assert_eq!(b["source_kind"], "file");
        assert_eq!(b["source_label"], "a.pdf");
        assert_eq!(b["store_id"], json!(leaf));
    }

    // a datatable job with steps on three slots, crawl-style children and credentials in its source
    let id = w.job(
        sid,
        "failed",
        json!({"kind": "datatable", "url": "https://data.example/t.csv?api_key=k-77",
               "auth": {"type": "bearer", "header_name": null},
               "credentials": {"token": "sekret-t6-xyz"}}),
        json!({"source_id": "src-t6-1"}),
        json!([
            {"kind": "acquire", "key": "", "status": "completed", "attempt": 1, "provider": null, "model": null,
             "error": null},
            {"kind": "ocr_pages", "key": "p0", "status": "failed", "attempt": 2, "provider": "mistral",
             "model": "ocr-1", "error": {"class": "provider", "code": "timeout", "message": "the provider timed out"}},
            {"kind": "caption", "key": "f1", "status": "completed", "attempt": 1, "provider": "openai",
             "model": "v-1", "error": null},
            {"kind": "transcribe_segment", "key": "s0001", "status": "pending", "attempt": 0, "provider": null,
             "model": null, "error": null},
        ]),
    );
    {
        let mut g = lock(&w.fake.ingest);
        let j = &mut g.jobs.last_mut().unwrap().1;
        j["children"] = json!({"total": 4, "by_status": {"completed": 3, "failed": 1}});
        j["document_ids"] = json!([Uuid::new_v4(), Uuid::new_v4()]);
        j["error"] = json!({"class": "provider", "code": "timeout", "message": "1 step failed"});
    }
    let (s, b) = w.read(&format!("/api/knowledge/ingest-jobs/{id}")).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["source_kind"], "datatable");
    assert_eq!(b["source_label"], "https://data.example/t.csv");
    assert_eq!(b["source_id"], "src-t6-1");
    assert_eq!(b["documents"], 2);
    assert_eq!(
        b["pages"],
        json!({"total": 4, "by_status": {"completed": 3, "failed": 1}})
    );
    let slots: Vec<Value> = b["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["slot"].clone())
        .collect();
    assert_eq!(
        slots,
        vec![
            json!(null),
            json!("ocr"),
            json!("vision"),
            json!("transcribe")
        ]
    );
    assert_eq!(b["steps"][1]["error"]["code"], "timeout");
    assert_eq!(b["steps"][1]["provider"], "mistral");
    assert_eq!(b["steps"][3]["status"], "queued");
    assert_eq!(
        b["error"],
        json!({"code": "timeout", "message": "1 step failed"})
    );
    assert_eq!(b["created_by"], "system");
    let text = b.to_string();
    assert!(!text.contains("sekret-t6-xyz"), "{text}");
    assert!(!text.contains("k-77"), "{text}");
    assert!(
        b.get("source").is_none() && b.get("options").is_none(),
        "{b}"
    );

    // an uploaded job names its uploader
    let up = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO knowledge_upload (tenant_id, id, store_id, language, ingest_job_ids, created_by) \
         VALUES ($1, $2, $3, 'en', '{}', 'u-alice-t6')",
    )
    .bind(w.tenant)
    .bind(up)
    .bind(leaf)
    .execute(&w.e.owner)
    .await
    .unwrap();
    let id = w.job(
        sid,
        "queued",
        json!({"kind": "upload", "filename": "b.pdf"}),
        json!({"source_id": format!("upload:{up}")}),
        json!([]),
    );
    let (s, b) = w.read(&format!("/api/knowledge/ingest-jobs/{id}")).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["created_by"], "u-alice-t6");

    // another tenant's job (or none) is strata's 404
    let (s, b) = w
        .read(&format!("/api/knowledge/ingest-jobs/{}", Uuid::new_v4()))
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    assert_eq!(code(&b), "not_found");
}

// ---------------------------------------------------------------------------------------------------------------
// Retry and cancel

#[tokio::test]
async fn retry_non_failed_409_conflict_passthrough() {
    let w = world().await;
    let sid = Uuid::new_v4();
    w.store("t6.retry.leaf", "leaf", None, Some(sid)).await;
    let id = w.upload_job(sid, "completed");
    let (s, b) = w
        .call(
            &w.km,
            "POST",
            &format!("/api/knowledge/ingest-jobs/{id}/retry"),
        )
        .await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(code(&b), "conflict");
    assert_eq!(w.audit_count("ingest.retried").await, 0);
    // an observer may read but not retry
    let (s, _) = w
        .call(
            &w.obs,
            "POST",
            &format!("/api/knowledge/ingest-jobs/{id}/retry"),
        )
        .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn retry_failed_step_with_empty_slot_409_slot_not_configured() {
    let w = world().await;
    let sid = Uuid::new_v4();
    w.store("t6.slot.leaf", "leaf", None, Some(sid)).await;
    let id = w.job(
        sid,
        "failed",
        json!({"kind": "upload", "filename": "scan.pdf"}),
        json!({}),
        json!([
            {"kind": "convert", "key": "", "status": "completed", "attempt": 1},
            {"kind": "ocr_pages", "key": "p0", "status": "failed", "attempt": 3,
             "error": {"code": "unsupported_no_provider", "message": "no ocr provider"}},
        ]),
    );
    sqlx::query(
        "INSERT INTO provider_slot (tenant_id, slot, service, configured, updated_by) \
         VALUES ($1, 'ocr', 'strata_ingest', false, 'test')",
    )
    .bind(w.tenant)
    .execute(&w.e.owner)
    .await
    .unwrap();
    let path = format!("/api/knowledge/ingest-jobs/{id}/retry");
    let (s, b) = w.call(&w.km, "POST", &path).await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(code(&b), "slot_not_configured");
    assert_eq!(b["error"]["slot"], "ocr");
    assert!(
        w.ingest_calls("POST", &format!("/api/ingest/jobs/{id}/retry"))
            .is_empty()
    );

    // configured: strata retries it
    sqlx::query("UPDATE provider_slot SET configured = true, kind = 'mistral', model = 'mistral-ocr-latest' WHERE tenant_id = $1 AND slot = 'ocr'")
        .bind(w.tenant)
        .execute(&w.e.owner)
        .await
        .unwrap();
    let (s, b) = w.call(&w.km, "POST", &path).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["status"], "queued");
    assert_eq!(b["id"], json!(id));
    assert_eq!(
        w.ingest_calls("POST", &format!("/api/ingest/jobs/{id}/retry"))
            .len(),
        1
    );
    assert_eq!(w.audit_count("ingest.retried").await, 1);
}

#[tokio::test]
async fn cancel_passthrough_200_cancelling() {
    let w = world().await;
    let sid = Uuid::new_v4();
    w.store("t6.cancel.leaf", "leaf", None, Some(sid)).await;
    let id = w.upload_job(sid, "running");
    let (s, b) = w
        .call(
            &w.km,
            "POST",
            &format!("/api/knowledge/ingest-jobs/{id}/cancel"),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["status"], "cancelling");
    assert_eq!(b["finished_at"], Value::Null);
    assert_eq!(w.audit_count("ingest.cancelled").await, 1);
}

#[tokio::test]
async fn cancel_completed_409_conflict() {
    let w = world().await;
    let sid = Uuid::new_v4();
    w.store("t6.cancel2.leaf", "leaf", None, Some(sid)).await;
    let id = w.upload_job(sid, "completed");
    let (s, b) = w
        .call(
            &w.km,
            "POST",
            &format!("/api/knowledge/ingest-jobs/{id}/cancel"),
        )
        .await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(code(&b), "conflict");
    assert_eq!(w.audit_count("ingest.cancelled").await, 0);
}

// ---------------------------------------------------------------------------------------------------------------
// Delete

#[tokio::test]
async fn delete_owned_by_running_source_run_409_owned_by_active_run_with_run_id() {
    let w = world().await;
    let sid = Uuid::new_v4();
    let leaf = w.store("t6.owned.leaf", "leaf", None, Some(sid)).await;
    let job = w.tenant_job("t6_owned", &["en"]).await;
    let parent = w.job(
        sid,
        "completed",
        json!({"kind": "crawl", "config": {"url": "https://site.example/"}}),
        json!({"source_id": "src"}),
        json!([]),
    );
    let page = w.job(
        sid,
        "completed",
        json!({"kind": "url", "url": "https://site.example/a", "parent_job_id": parent}),
        json!({"source_id": "src"}),
        json!([]),
    );
    let other = w.upload_job(sid, "completed");
    // T7's tables (0053): a website source with one running run that follows the crawl parent
    let source = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO source (tenant_id, job_id, id, name, type, config, mapping, target_store_id, enabled, language, \
                             updated_by) \
         VALUES ($1, $2, $3, 'site', 'website', '{\"urls\": [\"https://site.example/\"]}', '[]', $4, true, 'en', 'test')",
    )
    .bind(w.tenant)
    .bind(job)
    .bind(source)
    .bind(leaf)
    .execute(&w.e.owner)
    .await
    .unwrap();
    let run = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO source_run (tenant_id, job_id, id, source_id, status, started_by, ingest_job_ids) \
         VALUES ($1, $2, $3, $4, 'running', 'test', $5)",
    )
    .bind(w.tenant)
    .bind(job)
    .bind(run)
    .bind(source)
    .bind(vec![parent.to_string()])
    .execute(&w.e.owner)
    .await
    .unwrap();

    // the run's own job, and a page job of it
    for id in [parent, page] {
        let (s, b) = w
            .call(&w.km, "DELETE", &format!("/api/knowledge/ingest-jobs/{id}"))
            .await;
        assert_eq!(s, StatusCode::CONFLICT, "{b}");
        assert_eq!(code(&b), "owned_by_active_run");
        assert_eq!(b["error"]["run_id"], json!(run.to_string()));
        assert!(
            w.ingest_calls("DELETE", &format!("/api/ingest/jobs/{id}"))
                .is_empty()
        );
    }
    // a job no running run follows is deleted
    let (s, b) = w
        .call(
            &w.km,
            "DELETE",
            &format!("/api/knowledge/ingest-jobs/{other}"),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["documents"], "retained");
    // once the run has finished, its job is no longer held
    sqlx::query("UPDATE source_run SET status = 'completed', finished_at = now() WHERE id = $1")
        .bind(run)
        .execute(&w.e.owner)
        .await
        .unwrap();
    let (s, b) = w
        .call(
            &w.km,
            "DELETE",
            &format!("/api/knowledge/ingest-jobs/{parent}"),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

#[tokio::test]
async fn delete_child_409_child_of_parent_job_with_parent_job_id() {
    let w = world().await;
    let sid = Uuid::new_v4();
    w.store("t6.child.leaf", "leaf", None, Some(sid)).await;
    let parent = Uuid::new_v4();
    let page = w.job(
        sid,
        "completed",
        json!({"kind": "url", "url": "https://site.example/b", "parent_job_id": parent}),
        json!({}),
        json!([]),
    );
    let (s, b) = w
        .call(
            &w.km,
            "DELETE",
            &format!("/api/knowledge/ingest-jobs/{page}"),
        )
        .await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(code(&b), "child_of_parent_job");
    assert_eq!(b["error"]["parent_job_id"], json!(parent.to_string()));
    assert_eq!(w.audit_count("ingest.deleted").await, 0);
}

#[tokio::test]
async fn delete_202_deleting_passthrough() {
    let w = world().await;
    let sid = Uuid::new_v4();
    w.store("t6.slowdel.leaf", "leaf", None, Some(sid)).await;
    let id = w.upload_job(sid, "completed");
    lock(&w.fake.ingest).script(
        "DELETE",
        &format!("/api/ingest/jobs/{id}"),
        202,
        json!({"job_id": id, "status": "deleting"}),
    );
    let (s, b) = w
        .call(&w.km, "DELETE", &format!("/api/knowledge/ingest-jobs/{id}"))
        .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{b}");
    assert_eq!(b, json!({"job_id": id, "status": "deleting"}));
    assert_eq!(w.audit_count("ingest.deleted").await, 1);
}

#[tokio::test]
async fn delete_502_upstream_unavailable_passthrough() {
    let w = world().await;
    let sid = Uuid::new_v4();
    w.store("t6.blobfail.leaf", "leaf", None, Some(sid)).await;
    let id = w.upload_job(sid, "failed");
    lock(&w.fake.ingest).script(
        "DELETE",
        &format!("/api/ingest/jobs/{id}"),
        502,
        json!({"error": {"code": "upstream_unavailable", "message": "object storage did not answer"}}),
    );
    let (s, b) = w
        .call(&w.km, "DELETE", &format!("/api/knowledge/ingest-jobs/{id}"))
        .await;
    assert_eq!(s, StatusCode::BAD_GATEWAY, "{b}");
    assert_eq!(code(&b), "upstream_unavailable");
    assert_eq!(w.audit_count("ingest.deleted").await, 0);
}

// ---------------------------------------------------------------------------------------------------------------
// Direct upload

#[tokio::test]
async fn upload_to_group_422_not_a_leaf() {
    let w = world().await;
    let group = w
        .store("t6.up.group", "group", None, Some(Uuid::new_v4()))
        .await;
    let (s, b) = w.upload(group, &[file("a.pdf", b"%PDF-1.7")]).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(code(&b), "not_a_leaf");
    assert!(w.ingest_calls("POST", "/api/ingest/jobs").is_empty());
}

#[tokio::test]
async fn upload_unbound_409_store_not_bound() {
    let w = world().await;
    let leaf = w.store("t6.up.unbound", "leaf", None, None).await;
    let (s, b) = w.upload(leaf, &[file("a.pdf", b"%PDF-1.7")]).await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(code(&b), "store_not_bound");
    let (s, b) = w
        .upload(Uuid::new_v4(), &[file("a.pdf", b"%PDF-1.7")])
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    assert_eq!(code(&b), "store_not_found");
    assert!(w.ingest_calls("POST", "/api/ingest/jobs").is_empty());
}

#[tokio::test]
async fn upload_limits() {
    let w = world().await;
    let leaf = w
        .store("t6.up.limits", "leaf", None, Some(Uuid::new_v4()))
        .await;

    // 21 files
    let names: Vec<String> = (0..21).map(|i| format!("f{i}.txt")).collect();
    let many: Vec<Field<'_>> = names.iter().map(|n| file(n, b"x")).collect();
    let (s, b) = w.upload(leaf, &many).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(code(&b), "too_many_files");
    // 20 is the most, not too many
    let (s, b) = w.upload(leaf, &many[..20]).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let calls = w.ingest_calls("POST", "/api/ingest/jobs").len();
    assert_eq!(calls, 1);

    // 51 MB in all
    let big = vec![b'a'; 51 * 1024 * 1024];
    let (s, b) = w.upload(leaf, &[file("big.txt", &big)]).await;
    assert_eq!(s, StatusCode::PAYLOAD_TOO_LARGE, "{b}");
    assert_eq!(code(&b), "too_large");
    // also when split over files that are each small enough
    let half = vec![b'a'; 26 * 1024 * 1024];
    let (s, b) = w
        .upload(leaf, &[file("h1.txt", &half), file("h2.txt", &half)])
        .await;
    assert_eq!(s, StatusCode::PAYLOAD_TOO_LARGE, "{b}");
    assert_eq!(code(&b), "too_large");

    // a type outside the list
    let (s, b) = w
        .upload(leaf, &[file("a.pdf", b"%PDF"), file("setup.exe", b"MZ")])
        .await;
    assert_eq!(s, StatusCode::UNSUPPORTED_MEDIA_TYPE, "{b}");
    assert_eq!(code(&b), "unsupported_type");

    // no file at all
    let (s, b) = w.upload(leaf, &[("language", None, b"en".to_vec())]).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(code(&b), "no_file");

    // a bad language tag
    let (s, b) = w
        .upload(
            leaf,
            &[
                ("language", None, b"English".to_vec()),
                file("a.pdf", b"%PDF"),
            ],
        )
        .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(code(&b), "invalid_language");

    // none of the refusals reached strata_ingest
    assert_eq!(w.ingest_calls("POST", "/api/ingest/jobs").len(), calls);
}

#[tokio::test]
async fn upload_streams_multipart_with_upload_source_id_and_language() {
    let w = world().await;
    let sid = Uuid::new_v4();
    let leaf = w.store("t6.up.lang", "leaf", None, Some(sid)).await;
    w.tenant_job("t6_first", &["nl", "en"]).await;

    // no language given: the tenant's first job's first language
    let (s, b) = w
        .upload(leaf, &[file("handbook.pdf", b"%PDF-1.7 handbook")])
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let calls = w.ingest_calls("POST", "/api/ingest/jobs");
    assert_eq!(calls.len(), 1);
    let ct = calls[0].content_type.clone().unwrap_or_default();
    assert!(ct.starts_with("multipart/form-data"), "{ct}");
    assert!(calls[0].body_len > "%PDF-1.7 handbook".len());
    let UploadRow {
        id: up_id,
        language,
        files,
        ingest_job_ids: ids,
        ..
    } = w.first_upload().await;
    assert_eq!(language, "nl");
    assert_eq!(ids, vec![b["id"].as_str().unwrap().to_string()]);
    assert_eq!(files[0]["name"], "handbook.pdf");
    assert_eq!(files[0]["bytes"], 17);
    assert_eq!(files[0]["ingest_job_id"], b["id"]);
    {
        let g = lock(&w.fake.ingest);
        let job = &g.jobs.last().unwrap().1;
        assert_eq!(job["store_id"], json!(sid));
        assert_eq!(job["meta"]["source_id"], json!(format!("upload:{up_id}")));
        assert_eq!(job["options"]["language"], "nl");
    }
    assert_eq!(b["source_id"], json!(format!("upload:{up_id}")));
    assert_eq!(b["store_id"], json!(leaf));
    assert_eq!(b["source_kind"], "file");
    assert_eq!(w.audit_count("ingest.upload").await, 1);

    // a given language wins
    let (s, b) = w
        .upload(
            leaf,
            &[("language", None, b"de".to_vec()), file("faq.md", b"# FAQ")],
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let g = lock(&w.fake.ingest);
    assert_eq!(g.jobs.last().unwrap().1["options"]["language"], "de");
}

#[tokio::test]
async fn upload_returns_first_job_view_with_n_more_label() {
    let w = world().await;
    let sid = Uuid::new_v4();
    let leaf = w.store("t6.up.many", "leaf", None, Some(sid)).await;
    let (s, b) = w
        .upload(
            leaf,
            &[
                file("a.pdf", b"%PDF a"),
                file("b.txt", b"bee"),
                file("c.md", b"# c"),
            ],
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["source_label"], "a.pdf + 2 more");
    assert_eq!(b["status"], "queued");
    let created: Vec<Value> = lock(&w.fake.ingest)
        .jobs
        .iter()
        .filter(|(t, _)| *t == w.tenant)
        .map(|(_, j)| j.clone())
        .collect();
    assert_eq!(created.len(), 3);
    // the answer is the first file's job
    let first = created
        .iter()
        .find(|j| j["source"]["filename"] == "a.pdf")
        .unwrap();
    assert_eq!(b["id"], first["id"]);
    let UploadRow {
        ingest_job_ids: ids,
        files,
        created_by: by,
        ..
    } = w.first_upload().await;
    assert_eq!(ids.len(), 3);
    for j in &created {
        assert!(ids.contains(&j["id"].as_str().unwrap().to_string()), "{j}");
    }
    let names: Vec<&str> = files
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["a.pdf", "b.txt", "c.md"]);
    assert_eq!(b["created_by"], json!(by));
    // each job alone is one file
    let (s, l) = w
        .read(&format!("/api/knowledge/ingest-jobs?store_id={leaf}"))
        .await;
    assert_eq!(s, StatusCode::OK, "{l}");
    let mut labels: Vec<String> = l["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|j| j["source_label"].as_str().unwrap().to_string())
        .collect();
    labels.sort_unstable();
    assert_eq!(labels, vec!["a.pdf", "b.txt", "c.md"]);
}
