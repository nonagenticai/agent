//! B3 T8: source re-sync runs (plan §5.4 `resyncSource` / `sourceRuns` / `sourceRun`): the start through strata_ingest
//! (the key inline for that one request, never stored), the refresh when a run is READ, the replace of earlier runs'
//! documents in strata_rag (one delete per document, never the `except_run` prune; documents another source or a
//! direct upload shares are kept; the empty-sync guard), `source_run` sent for provenance, the run detail, the re-run
//! of a failed feed job (T6 `retryIngestJob`) and the store delete that a source holds (409 `store_has_sources`).
//! Real Postgres 18 (one container for this binary, a fresh database per test) and the in-process strata fakes
//! (`common::strata_fake`).
use crate::common;

use std::collections::HashMap;

use crate::common::strata_fake::{self, StrataFake, lock};
use crate::common::{TestEnv, get, insert_tenant, insert_user, json, person, send};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::Engine as _;
use serde_json::{Value, json};
use uuid::Uuid;

const JOB: &str = "support";
/// The key of the api / datatable sources.
const KEY: &str = "sk-run-secret-0001-abcdefghij";
const BOUNDARY: &str = "t8-boundary-91fe";

fn with_keys(vars: &mut HashMap<String, String>) {
    let key = base64::engine::general_purpose::STANDARD.encode([9u8; 32]);
    vars.insert(
        "PROVIDER_SECRET_KEYS".to_string(),
        format!(r#"{{"s1":"{key}"}}"#),
    );
    vars.insert("PROVIDER_SECRET_KEY_CURRENT".to_string(), "s1".to_string());
}

struct W {
    e: TestEnv,
    fake: StrataFake,
    tenant: Uuid,
    /// The bound leaf (agent id) and its strata store.
    leaf: Uuid,
    sid: Uuid,
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

async fn add_job(e: &TestEnv, tenant: Uuid, key: &str) {
    sqlx::query(
        "INSERT INTO jobs (tenant_id, key, label, pins, languages, created_by) \
         VALUES ($1, $2, $2, '[]', ARRAY['nl', 'en'], 'test')",
    )
    .bind(tenant)
    .bind(key)
    .execute(&e.owner)
    .await
    .unwrap();
}

/// A fresh tenant with the job `support` (nl, en), a leaf `faq.policy` bound to a strata store, the keyring and the
/// strata fakes.
async fn world() -> W {
    let fake = strata_fake::start().await;
    let tenant = Uuid::new_v4();
    let e = common::env_with(|v| {
        fake.env(&[tenant], v);
        with_keys(v);
    })
    .await;
    insert_tenant(&e.owner, tenant, &format!("T8 {tenant}")).await;
    add_job(&e, tenant, JOB).await;
    let sid = lock(&fake.rag).add_store(tenant, None, "leaf", "faq.policy");
    let leaf = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO knowledge_store (tenant_id, id, key, label, kind, strata_store_id, region, updated_by) \
         VALUES ($1, $2, 'faq.policy', 'FAQ policy', 'leaf', $3, 'eu', 'test')",
    )
    .bind(tenant)
    .bind(leaf)
    .bind(sid)
    .execute(&e.owner)
    .await
    .unwrap();
    let km = member(&e, tenant, "knowledge_manager").await;
    let obs = member(&e, tenant, "tenant_user").await;
    W {
        e,
        fake,
        tenant,
        leaf,
        sid,
        km,
        obs,
    }
}

fn code(b: &Value) -> &str {
    b["error"]["code"].as_str().unwrap_or("")
}

fn src_uri(job: &str, rest: &str) -> String {
    format!("/api/jobs/{job}/sources{rest}")
}

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

/// A strata_rag document of `source` in `store` (the fake), optionally stamped with a run.
fn rag_doc(id: &str, store: Uuid, source: &str, title: &str, run: Option<&str>) -> Value {
    json!({
        "id": id, "store_id": store, "source": format!("{title}.html"), "title": title, "content_hash": id,
        "token_count": 100, "chunk_count": 2, "source_id": source, "source_run": run, "metadata": {},
        "created_at": "2026-10-21T10:00:00Z"
    })
}

impl W {
    async fn call(&self, tok: &str, method: &str, uri: &str, body: Value) -> (StatusCode, Value) {
        send(&self.e.app, json(method, uri, Some(tok), body)).await
    }

    async fn read(&self, uri: &str) -> (StatusCode, Value) {
        send(&self.e.app, get(uri, Some(&self.obs))).await
    }

    fn website(&self, name: &str) -> Value {
        json!({
            "name": name, "type": "website", "config": {"urls": ["https://help.example.nl/returns"]},
            "mapping": [], "target_store_id": self.leaf, "schedule": null, "enabled": true
        })
    }

    fn api(&self, name: &str) -> Value {
        json!({
            "name": name, "type": "api",
            "config": {"url": "https://api.example.com/promos", "auth": {"type": "bearer"}},
            "mapping": [], "target_store_id": self.leaf, "schedule": null, "enabled": true,
            "credentials": {"secret": KEY}
        })
    }

    fn datatable(&self, name: &str) -> Value {
        json!({
            "name": name, "type": "datatable",
            "config": {"url": "https://data.example.com/products.csv", "format": "csv", "auth": {"type": "bearer"}},
            "mapping": [], "target_store_id": self.leaf, "schedule": null, "enabled": true,
            "credentials": {"secret": KEY}
        })
    }

    fn file_source(&self, name: &str) -> Value {
        json!({
            "name": name, "type": "file", "config": {}, "mapping": [], "target_store_id": self.leaf,
            "schedule": null, "enabled": true
        })
    }

    /// Creates `body` in `job` as the knowledge_manager; its id.
    async fn create_in(&self, job: &str, body: Value) -> String {
        let (s, b) = self.call(&self.km, "POST", &src_uri(job, ""), body).await;
        assert_eq!(s, StatusCode::CREATED, "{b}");
        b["id"].as_str().unwrap().to_string()
    }

    async fn create(&self, body: Value) -> String {
        self.create_in(JOB, body).await
    }

    async fn resync(&self, id: &str, body: Value) -> (StatusCode, Value) {
        self.call(
            &self.km,
            "POST",
            &src_uri(JOB, &format!("/{id}/resync")),
            body,
        )
        .await
    }

    async fn resync_files(&self, id: &str, fields: &[Field<'_>]) -> (StatusCode, Value) {
        let req = Request::post(src_uri(JOB, &format!("/{id}/resync")))
            .header(
                "content-type",
                format!("multipart/form-data; boundary={BOUNDARY}"),
            )
            .header("authorization", format!("Bearer {}", self.km))
            .body(Body::from(form(fields)))
            .unwrap();
        send(&self.e.app, req).await
    }

    async fn runs(&self, id: &str) -> Value {
        let (s, b) = self.read(&src_uri(JOB, &format!("/{id}/runs"))).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        b
    }

    /// Sets strata_ingest job `job` to `status` with `docs`.
    fn settle(&self, job: &str, status: &str, docs: &[&str]) {
        let mut g = lock(&self.fake.ingest);
        let j = g
            .jobs
            .iter_mut()
            .find(|(_, j)| j["id"].as_str() == Some(job))
            .map(|(_, j)| j)
            .expect("the job is in the fake");
        j["status"] = json!(status);
        j["document_ids"] = json!(docs);
        if status == "failed" {
            j["error"] = json!({"class": "source", "code": "source_auth", "message": "HTTP 401 from the source"});
        }
    }

    fn put_doc(&self, d: Value) {
        lock(&self.fake.rag).documents.push((self.tenant, d));
    }

    fn has_doc(&self, id: &str) -> bool {
        lock(&self.fake.rag)
            .documents
            .iter()
            .any(|(_, d)| d["id"].as_str() == Some(id))
    }

    fn ingest_creates(&self) -> Vec<strata_fake::Recorded> {
        self.fake
            .ingest_requests()
            .into_iter()
            .filter(|r| r.method == "POST" && r.path == "/api/ingest/jobs")
            .collect()
    }

    fn rag_calls(&self, method: &str, prefix: &str) -> Vec<strata_fake::Recorded> {
        self.fake
            .rag_requests()
            .into_iter()
            .filter(|r| r.method == method && r.path.starts_with(prefix))
            .collect()
    }

    async fn scalar_i64(&self, sql: &'static str, id: &str) -> i64 {
        sqlx::query_scalar(sql)
            .bind(id)
            .fetch_one(&self.e.owner)
            .await
            .unwrap()
    }

    /// Every text of the tenant's audit rows and of the source's run rows.
    async fn stored_text(&self) -> String {
        let audit: Vec<String> =
            sqlx::query_scalar("SELECT details::text FROM audit_event WHERE tenant_id = $1")
                .bind(self.tenant)
                .fetch_all(&self.e.owner)
                .await
                .unwrap();
        let runs: Vec<String> = sqlx::query_scalar(
            "SELECT row_to_json(r)::text FROM source_run r WHERE tenant_id = $1",
        )
        .bind(self.tenant)
        .fetch_all(&self.e.owner)
        .await
        .unwrap();
        format!("{}\n{}", audit.join("\n"), runs.join("\n"))
    }

    /// One completed run of website source `id` producing `docs`; the run.
    async fn completed_run(&self, id: &str, docs: &[&str]) -> Value {
        let (s, run) = self.resync(id, Value::Null).await;
        assert_eq!(s, StatusCode::ACCEPTED, "{run}");
        let job = run["ingest_job_ids"][0].as_str().unwrap().to_string();
        self.settle(&job, "completed", docs);
        let runs = self.runs(id).await;
        let now = runs["items"][0].clone();
        assert_eq!(now["id"], run["id"]);
        assert_eq!(now["status"], "completed", "{now}");
        now
    }
}

fn doc_id() -> String {
    Uuid::new_v4().to_string()
}

// ---- start --------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn resync_disabled_409_source_disabled() {
    let w = world().await;
    let mut body = w.website("Help centre");
    body["enabled"] = json!(false);
    let id = w.create(body).await;
    let (s, b) = w.resync(&id, Value::Null).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "source_disabled"),
        "{b}"
    );
    assert!(
        w.ingest_creates().is_empty(),
        "nothing sent to strata_ingest"
    );
    assert_eq!(
        w.scalar_i64(
            "SELECT count(*) FROM source_run WHERE source_id = $1::uuid",
            &id
        )
        .await,
        0
    );
    // a member without knowledge_manager may not re-sync
    let (s, b) = w
        .call(
            &w.obs,
            "POST",
            &src_uri(JOB, &format!("/{id}/resync")),
            Value::Null,
        )
        .await;
    assert_eq!((s, code(&b)), (StatusCode::FORBIDDEN, "forbidden"), "{b}");
}

#[tokio::test]
async fn resync_while_running_409_run_in_progress() {
    let w = world().await;
    let id = w.create(w.website("Help centre")).await;
    let (s, run) = w.resync(&id, Value::Null).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{run}");
    assert_eq!(run["status"], "running");
    // the job is still queued in strata_ingest: the refresh keeps the run running
    let (s, b) = w.resync(&id, Value::Null).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "run_in_progress"),
        "{b}"
    );
    assert_eq!(w.ingest_creates().len(), 1, "no second job");
    let (s, b) = w
        .call(
            &w.km,
            "DELETE",
            &src_uri(JOB, &format!("/{id}")),
            Value::Null,
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "run_in_progress"),
        "{b}"
    );
    // once its job ended, the next re-sync (or delete) finishes it first and goes ahead
    let job = run["ingest_job_ids"][0].as_str().unwrap().to_string();
    w.settle(&job, "completed", &[&doc_id()]);
    let (s, b) = w.resync(&id, Value::Null).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{b}");
    assert_eq!(w.runs(&id).await["items"][1]["status"], "completed");
}

#[tokio::test]
async fn resync_feed_creates_one_ingest_job_with_inline_credentials() {
    let w = world().await;
    let id = w.create(w.api("Promotions")).await;
    let (s, run) = w.resync(&id, Value::Null).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{run}");
    assert_eq!(run["status"], "running");
    assert_eq!(run["mapping"], "same");
    assert_eq!(run["uploads"], Value::Null);
    assert_eq!(run["ingest_job_ids"].as_array().unwrap().len(), 1);
    let creates = w.ingest_creates();
    assert_eq!(creates.len(), 1, "one ingest job for a feed");
    let body = &creates[0].body;
    assert_eq!(body["store_id"], json!(w.sid), "the leaf's strata store");
    assert_eq!(body["source_id"], json!(id), "sources::strata_source_id");
    assert_eq!(
        body["source_run"], run["id"],
        "the run's id beside source_id (strata_ingest #14)"
    );
    assert_eq!(body["source"]["kind"], "api");
    assert_eq!(
        body["source"]["config"]["url"],
        "https://api.example.com/promos"
    );
    assert_eq!(
        body["source"]["credentials"],
        json!({"secret": KEY}),
        "the key inline, for this request"
    );
    assert_eq!(body["mapping"], json!([]));
    assert_eq!(
        body["options"]["language"], "nl",
        "the source's language (D68)"
    );
    // never stored, never answered
    assert!(!run.to_string().contains(KEY), "{run}");
    let stored = w.stored_text().await;
    assert!(
        !stored.contains(KEY),
        "the key is in no run row or audit row"
    );
    let audit: Value = sqlx::query_scalar(
        "SELECT details FROM audit_event WHERE action = 'source.resync' AND resource_id = $1",
    )
    .bind(&id)
    .fetch_one(&w.e.owner)
    .await
    .unwrap();
    assert_eq!(audit["status"], "running");
    assert_eq!(audit["trigger"], "manual");
    assert_eq!(audit["jobs"], 1);
    let (_, list) = w.read(&src_uri(JOB, &format!("/{id}"))).await;
    assert_eq!(list["last_run"]["id"], run["id"]);
}

#[tokio::test]
async fn resync_edited_mapping_validates_and_stores() {
    let w = world().await;
    let id = w.create(w.datatable("Products")).await;
    // a stored mapping (B5 provides the targets; written as the owner here)
    let stored = json!([{"source_field": "sku", "entity": "Offering", "attribute": "sku"}]);
    sqlx::query("UPDATE source SET mapping = $2 WHERE id = $1::uuid")
        .bind(&id)
        .bind(&stored)
        .execute(&w.e.owner)
        .await
        .unwrap();
    // before B5 no attribute exists: an edited row is refused, a duplicate first
    let row = |f: &str| json!({"source_field": f, "entity": "Offering", "attribute": "title"});
    let (s, b) = w
        .resync(&id, json!({"mapping": [row("title"), row("Title")]}))
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "duplicate_field"),
        "{b}"
    );
    let (s, b) = w.resync(&id, json!({"mapping": [row("title")]})).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "unknown_attribute"),
        "{b}"
    );
    assert!(
        w.ingest_creates().is_empty(),
        "a refused mapping starts nothing"
    );
    // the stored mapping goes out as is
    let (s, run) = w.resync(&id, json!({})).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{run}");
    assert_eq!(run["mapping"], "same");
    assert_eq!(w.ingest_creates()[0].body["mapping"], stored);
    let job = run["ingest_job_ids"][0].as_str().unwrap().to_string();
    w.settle(&job, "completed", &[&doc_id()]);
    // an edited (blank) mapping: stored on the source and sent
    let (s, run) = w
        .resync(
            &id,
            json!({"mapping": [{"source_field": " ", "entity": "", "attribute": ""}]}),
        )
        .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{run}");
    assert_eq!(run["mapping"], "edited");
    assert_eq!(w.ingest_creates()[1].body["mapping"], json!([]));
    let (_, src) = w.read(&src_uri(JOB, &format!("/{id}"))).await;
    assert_eq!(src["mapping"], json!([]), "the edited mapping is stored");
    // a mapping on a website source is not a thing
    let web = w.create(w.website("Help centre")).await;
    let (s, b) = w.resync(&web, json!({"mapping": []})).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_source"),
        "{b}"
    );
}

#[tokio::test]
async fn resync_file_source_one_job_per_file_and_per_file_errors() {
    let w = world().await;
    let id = w.create(w.file_source("Manuals")).await;
    // limits
    let (s, b) = w
        .resync_files(&id, &[("note", None, b"no file".to_vec())])
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "no_file"),
        "{b}"
    );
    let names: Vec<String> = (0..21).map(|i| format!("m{i}.pdf")).collect();
    let many: Vec<Field<'_>> = names.iter().map(|n| file(n, b"%PDF-1.7")).collect();
    let (s, b) = w.resync_files(&id, &many).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "too_many_files"),
        "{b}"
    );
    assert!(w.ingest_creates().is_empty());
    // two files strata_ingest reads, one it does not
    let (s, run) = w
        .resync_files(
            &id,
            &[
                file("returns.pdf", b"%PDF-1.7 returns"),
                file("setup.exe", b"MZ\x90\x00"),
                file("care.docx", b"PK\x03\x04 docx"),
            ],
        )
        .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{run}");
    let creates = w.ingest_creates();
    assert_eq!(creates.len(), 1, "one multipart request");
    assert!(
        creates[0]
            .content_type
            .as_deref()
            .unwrap_or_default()
            .starts_with("multipart/form-data"),
    );
    let jobs = run["ingest_job_ids"].as_array().unwrap().clone();
    assert_eq!(jobs.len(), 2, "one ingest job per accepted file: {run}");
    let uploads = run["uploads"].as_array().unwrap();
    assert_eq!(uploads.len(), 3);
    assert_eq!(uploads[0]["file_name"], "returns.pdf");
    assert_eq!(uploads[0]["ingest_job_id"], jobs[0]);
    assert_eq!(uploads[0]["error"], Value::Null);
    assert_eq!(uploads[1]["file_name"], "setup.exe");
    assert_eq!(uploads[1]["ingest_job_id"], Value::Null);
    assert_eq!(uploads[1]["error"]["code"], "unsupported_type");
    assert_eq!(uploads[1]["bytes"], 4);
    assert_eq!(uploads[2]["ingest_job_id"], jobs[1]);
    assert_eq!(run["mapping"], Value::Null);
    // `source_run` went as a field before the files (the fake refuses it after one) into every job's meta
    {
        let g = lock(&w.fake.ingest);
        for j in &jobs {
            let (_, v) = g
                .jobs
                .iter()
                .find(|(_, v)| v["id"] == *j)
                .expect("the job is in the fake");
            assert_eq!(v["meta"]["source_run"], run["id"], "{v}");
            assert_eq!(v["meta"]["source_id"], json!(id), "{v}");
        }
    }
    // every accepted file ingested, one refused: the run fails, its documents are added, nothing is replaced
    w.settle(jobs[0].as_str().unwrap(), "completed", &[&doc_id()]);
    w.settle(jobs[1].as_str().unwrap(), "completed", &[&doc_id()]);
    let now = w.runs(&id).await["items"][0].clone();
    assert_eq!(now["status"], "failed", "{now}");
    assert_eq!(now["error"]["code"], "partial_upload");
    assert_eq!(now["documents"], 2);
    assert_eq!(now["replace_report"], Value::Null);
    // only refused files: nothing is sent and the run fails at once
    let (s, run) = w.resync_files(&id, &[file("setup.exe", b"MZ")]).await;
    assert_eq!(s, StatusCode::OK, "{run}");
    assert_eq!(run["status"], "failed");
    assert_eq!(run["error"]["code"], "nothing_ingested");
    assert_eq!(w.ingest_creates().len(), 1, "nothing sent for it");
}

#[tokio::test]
async fn resync_unsupported_source_kind_ends_the_run_unsupported() {
    let w = world().await;
    let id = w.create(w.api("Promotions")).await;
    lock(&w.fake.ingest).script(
        "POST",
        "/api/ingest/jobs",
        422,
        json!({"error": {"code": "unsupported_source_kind", "message": "unsupported source kind: api"}}),
    );
    let (s, run) = w.resync(&id, Value::Null).await;
    assert_eq!(s, StatusCode::OK, "{run}");
    assert_eq!(run["status"], "unsupported");
    assert_eq!(run["error"]["code"], "unsupported_source_kind");
    assert!(run["finished_at"].is_string());
    // a 5xx does not prove a refusal: the run fails and the request answers 503
    lock(&w.fake.ingest).script(
        "POST",
        "/api/ingest/jobs",
        500,
        json!({"error": {"code": "boom", "message": "x"}}),
    );
    let (s, b) = w.resync(&id, Value::Null).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::SERVICE_UNAVAILABLE, "unavailable"),
        "{b}"
    );
    let last = w.runs(&id).await["items"][0].clone();
    assert_eq!(last["status"], "failed");
    assert_eq!(last["error"]["code"], "ingest_unconfirmed");
}

#[tokio::test]
async fn resync_unbound_store_409_store_not_bound() {
    let w = world().await;
    let unbound = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO knowledge_store (tenant_id, id, key, label, kind, region, updated_by) \
         VALUES ($1, $2, 'faq.new', 'New', 'leaf', 'eu', 'test')",
    )
    .bind(w.tenant)
    .bind(unbound)
    .execute(&w.e.owner)
    .await
    .unwrap();
    let mut body = w.website("Help centre");
    body["target_store_id"] = json!(unbound);
    let id = w.create(body).await;
    let (s, b) = w.resync(&id, Value::Null).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "store_not_bound"),
        "{b}"
    );
    assert!(w.ingest_creates().is_empty());
}

// ---- refresh on read ----------------------------------------------------------------------------------------------------

#[tokio::test]
async fn run_refresh_on_read_completes_when_jobs_end() {
    let w = world().await;
    let id = w.create(w.website("Help centre")).await;
    let (_, run) = w.resync(&id, Value::Null).await;
    let job = run["ingest_job_ids"][0].as_str().unwrap().to_string();
    let runs = w.runs(&id).await;
    assert_eq!(
        runs["items"][0]["status"], "running",
        "the job is still queued"
    );
    assert_eq!(runs["items"][0]["finished_at"], Value::Null);
    let (a, b) = (doc_id(), doc_id());
    w.settle(&job, "completed", &[&a, &b, &a]);
    let runs = w.runs(&id).await;
    let now = &runs["items"][0];
    assert_eq!(now["status"], "completed", "{now}");
    assert_eq!(now["documents"], 2, "documents counted once each");
    assert!(now["finished_at"].is_string());
    assert_eq!(now["error"], Value::Null);
    let ids: Vec<String> = sqlx::query_scalar(
        "SELECT unnest(document_ids) FROM source_run WHERE id = $1::uuid ORDER BY 1",
    )
    .bind(run["id"].as_str().unwrap())
    .fetch_all(&w.e.owner)
    .await
    .unwrap();
    let mut want = vec![a, b];
    want.sort();
    assert_eq!(ids, want);
    // a failed feed job: the run fails with the job's own code, scrubbed of the key
    let api = w.create(w.api("Promotions")).await;
    let (_, run) = w.resync(&api, Value::Null).await;
    let job = run["ingest_job_ids"][0].as_str().unwrap().to_string();
    {
        let mut g = lock(&w.fake.ingest);
        let j = g
            .jobs
            .iter_mut()
            .find(|(_, j)| j["id"].as_str() == Some(job.as_str()))
            .map(|(_, j)| j)
            .unwrap();
        j["status"] = json!("failed");
        j["error"] = json!({"code": "source_auth", "message": format!("HTTP 401: the key {KEY} was rejected")});
    }
    let (s, detail) = w
        .read(&src_uri(
            JOB,
            &format!("/{api}/runs/{}", run["id"].as_str().unwrap()),
        ))
        .await;
    assert_eq!(s, StatusCode::OK, "{detail}");
    assert_eq!(detail["run"]["status"], "failed");
    assert_eq!(detail["run"]["error"]["code"], "source_auth");
    assert!(!detail.to_string().contains(KEY), "{detail}");
    assert!(!w.stored_text().await.contains(KEY));
}

#[tokio::test]
async fn crawl_run_follows_its_page_jobs() {
    let w = world().await;
    let mut body = w.website("Help centre");
    body["config"] =
        json!({"mode": "crawl", "url": "https://help.example.nl/", "crawl": {"max_depth": 2}});
    let id = w.create(body).await;
    let (_, run) = w.resync(&id, Value::Null).await;
    let parent = run["ingest_job_ids"][0].as_str().unwrap().to_string();
    assert_eq!(w.ingest_creates()[0].body["source"]["kind"], "crawl");
    let child = Uuid::new_v4().to_string();
    {
        let mut g = lock(&w.fake.ingest);
        let tenant = w.tenant;
        let p = g
            .jobs
            .iter_mut()
            .find(|(_, j)| j["id"].as_str() == Some(parent.as_str()))
            .map(|(_, j)| j)
            .unwrap();
        p["status"] = json!("completed");
        p["children"] = json!({"total": 1, "by_status": {"running": 1}});
        p["child_job_ids"] = json!([child]);
        g.jobs.push((
            tenant,
            json!({"id": child, "status": "running", "document_ids": [], "source": {"kind": "url",
                   "parent_job_id": parent}, "meta": {"source_id": id}, "steps": []}),
        ));
    }
    let now = w.runs(&id).await["items"][0].clone();
    assert_eq!(now["status"], "running", "a page job is still running");
    assert_eq!(
        now["pages"]["total"], 1,
        "the page summary is kept while it runs"
    );
    let d = doc_id();
    {
        let mut g = lock(&w.fake.ingest);
        for (_, j) in g.jobs.iter_mut() {
            if j["id"].as_str() == Some(child.as_str()) {
                j["status"] = json!("completed");
                j["document_ids"] = json!([d]);
            }
            if j["id"].as_str() == Some(parent.as_str()) {
                j["children"] = json!({"total": 1, "by_status": {"completed": 1}});
            }
        }
    }
    let now = w.runs(&id).await["items"][0].clone();
    assert_eq!(now["status"], "completed", "{now}");
    assert_eq!(now["documents"], 1, "the page job's document");
}

#[tokio::test]
async fn orphaned_run_without_jobs_fails_on_read() {
    let w = world().await;
    let id = w.create(w.website("Help centre")).await;
    sqlx::query(
        "INSERT INTO source_run (tenant_id, job_id, id, source_id, status, started_at, started_by) \
         SELECT $1, j.id, $2, $3::uuid, 'running', now() - interval '5 minutes', 'test' \
           FROM jobs j WHERE j.tenant_id = $1 AND j.key = $4",
    )
    .bind(w.tenant)
    .bind(Uuid::new_v4())
    .bind(&id)
    .bind(JOB)
    .execute(&w.e.owner)
    .await
    .unwrap();
    let now = w.runs(&id).await["items"][0].clone();
    assert_eq!(now["status"], "failed");
    assert_eq!(now["error"]["code"], "run_orphaned");
}

// ---- replace ------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn completed_run_replaces_earlier_documents() {
    let w = world().await;
    let id = w.create(w.website("Help centre")).await;
    let (a, b, c, gone) = (doc_id(), doc_id(), doc_id(), doc_id());
    w.put_doc(rag_doc(&a, w.sid, &id, "Returns", None));
    w.put_doc(rag_doc(&b, w.sid, &id, "Shipping", None));
    let first = w.completed_run(&id, &[&a, &b, &gone]).await;
    assert_eq!(
        first["replace_report"],
        json!({"deleted": 0, "already_gone": 0, "kept_shared": 0, "failed": []}),
        "nothing earlier to replace"
    );
    // run 2 produces b again and c: a is deleted, `gone` was deleted by hand, b is kept
    w.put_doc(rag_doc(&c, w.sid, &id, "Care", None));
    let second = w.completed_run(&id, &[&b, &c]).await;
    assert_eq!(
        second["replace_report"],
        json!({"deleted": 1, "already_gone": 1, "kept_shared": 1, "failed": []}),
        "{second}"
    );
    assert!(!w.has_doc(&a) && w.has_doc(&b) && w.has_doc(&c));
    // one delete per document; strata_rag's `except_run` prune is never asked
    let run2 = second["id"].as_str().unwrap();
    assert!(
        w.rag_calls("DELETE", "/api/sources/").is_empty(),
        "no prune"
    );
    assert_eq!(
        w.rag_calls("DELETE", &format!("/api/documents/{a}")).len(),
        1
    );
    assert_eq!(
        w.scalar_i64(
            "SELECT count(*) FROM source_run WHERE id = $1::uuid AND documents_removed",
            first["id"].as_str().unwrap()
        )
        .await,
        1,
        "the first run's documents are gone"
    );
    let replaced: Option<Uuid> =
        sqlx::query_scalar("SELECT replaced_run_id FROM source_run WHERE id = $1::uuid")
            .bind(run2)
            .fetch_one(&w.e.owner)
            .await
            .unwrap();
    assert_eq!(
        replaced.map(|u| u.to_string()).as_deref(),
        first["id"].as_str()
    );
    let audit: Value = sqlx::query_scalar(
        "SELECT details FROM audit_event WHERE action = 'source.replace' AND resource_id = $1 ORDER BY id DESC LIMIT 1",
    )
    .bind(&id)
    .fetch_one(&w.e.owner)
    .await
    .unwrap();
    assert_eq!(audit["deleted"], 1);
    // documents stamped with the run (strata_ingest #14) change nothing: still one delete per document
    let (s, run) = w.resync(&id, Value::Null).await;
    assert_eq!(s, StatusCode::ACCEPTED);
    let run3 = run["id"].as_str().unwrap().to_string();
    let e = doc_id();
    {
        let mut g = lock(&w.fake.rag);
        for (_, d) in g.documents.iter_mut() {
            if d["id"].as_str() == Some(c.as_str()) {
                d["source_run"] = json!(run3);
            }
        }
    }
    w.put_doc(rag_doc(&e, w.sid, &id, "Warranty", Some(&run3)));
    w.settle(
        run["ingest_job_ids"][0].as_str().unwrap(),
        "completed",
        &[&c, &e],
    );
    let third = w.runs(&id).await["items"][0].clone();
    assert_eq!(third["status"], "completed");
    assert_eq!(
        third["replace_report"],
        json!({"deleted": 1, "already_gone": 0, "kept_shared": 1, "failed": []}),
        "{third}"
    );
    assert!(!w.has_doc(&b) && w.has_doc(&c) && w.has_doc(&e));
    assert_eq!(
        w.rag_calls("DELETE", &format!("/api/documents/{b}")).len(),
        1
    );
    assert!(
        w.rag_calls("DELETE", "/api/sources/").is_empty(),
        "still no prune"
    );
}

#[tokio::test]
async fn replace_keeps_documents_another_source_holds() {
    let w = world().await;
    let a = w.create(w.website("Help centre")).await;
    let other = w.create(w.website("Help centre mirror")).await;
    // strata dedups on (store, content): the shared document carries the first writer's source_id (a)
    let (shared, own) = (doc_id(), doc_id());
    w.put_doc(rag_doc(&shared, w.sid, &a, "Returns", None));
    w.put_doc(rag_doc(&own, w.sid, &a, "Shipping", None));
    w.completed_run(&a, &[&shared, &own]).await;
    w.completed_run(&other, &[&shared]).await;
    // a's next run has neither: own is deleted, the one `other` still holds stays
    let fresh = doc_id();
    w.put_doc(rag_doc(&fresh, w.sid, &a, "Care", None));
    let next = w.completed_run(&a, &[&fresh]).await;
    assert_eq!(
        next["replace_report"],
        json!({"deleted": 1, "already_gone": 0, "kept_shared": 1, "failed": []}),
        "{next}"
    );
    assert!(w.has_doc(&shared) && !w.has_doc(&own) && w.has_doc(&fresh));
    assert!(
        w.rag_calls("DELETE", &format!("/api/documents/{shared}"))
            .is_empty()
    );
    assert!(
        w.rag_calls("DELETE", "/api/sources/").is_empty(),
        "no prune"
    );
}

#[tokio::test]
async fn replace_keeps_documents_a_direct_upload_shares() {
    let w = world().await;
    let id = w.create(w.website("Help centre")).await;
    let (shared, own) = (doc_id(), doc_id());
    w.put_doc(rag_doc(&shared, w.sid, &id, "Returns", None));
    w.put_doc(rag_doc(&own, w.sid, &id, "Shipping", None));
    w.completed_run(&id, &[&shared, &own]).await;
    // a direct upload into the same leaf deduplicated onto `shared` (its ingest job names it)
    let job = Uuid::new_v4();
    lock(&w.fake.ingest).jobs.push((
        w.tenant,
        json!({
            "id": job, "tenant": w.tenant, "store_id": w.sid,
            "source": {"kind": "upload", "filename": "returns.pdf"}, "options": {},
            "meta": {"source_id": format!("upload:{}", Uuid::new_v4())}, "status": "completed", "error": null,
            "document_ids": [shared], "created_at": "2026-10-21T10:00:00Z",
            "updated_at": "2026-10-21T10:00:00Z", "steps": [],
        }),
    ));
    sqlx::query(
        "INSERT INTO knowledge_upload (tenant_id, id, store_id, language, files, ingest_job_ids, created_by) \
         VALUES ($1, $2, $3, 'nl', '[]', ARRAY[$4::text], 'test')",
    )
    .bind(w.tenant)
    .bind(Uuid::new_v4())
    .bind(w.leaf)
    .bind(job.to_string())
    .execute(&w.e.owner)
    .await
    .unwrap();
    let fresh = doc_id();
    w.put_doc(rag_doc(&fresh, w.sid, &id, "Care", None));
    let next = w.completed_run(&id, &[&fresh]).await;
    assert_eq!(
        next["replace_report"],
        json!({"deleted": 1, "already_gone": 0, "kept_shared": 1, "failed": []}),
        "{next}"
    );
    assert!(w.has_doc(&shared), "the upload's document stays");
    assert!(!w.has_doc(&own));
    assert!(
        w.rag_calls("DELETE", &format!("/api/documents/{shared}"))
            .is_empty()
    );
}

#[tokio::test]
async fn empty_completed_run_replaces_nothing() {
    let w = world().await;
    let id = w.create(w.website("Help centre")).await;
    let a = doc_id();
    w.put_doc(rag_doc(&a, w.sid, &id, "Returns", None));
    w.completed_run(&id, &[&a]).await;
    let prunes_before = w.rag_calls("DELETE", "/api/sources/").len();
    // the source answered nothing this time: the run completes and its report deletes nothing
    let empty = w.completed_run(&id, &[]).await;
    assert_eq!(
        empty["replace_report"],
        json!({"deleted": 0, "already_gone": 0, "kept_shared": 0, "failed": []}),
        "{empty}"
    );
    assert!(w.has_doc(&a), "the earlier document stays");
    assert_eq!(
        w.rag_calls("DELETE", "/api/sources/").len(),
        prunes_before,
        "no prune"
    );
    // the same document again: kept (this run has it), nothing deleted, the run still completed
    let same = w.completed_run(&id, &[&a]).await;
    assert_eq!(
        same["replace_report"],
        json!({"deleted": 0, "already_gone": 0, "kept_shared": 1, "failed": []}),
        "{same}"
    );
    assert!(w.has_doc(&a));
}

#[tokio::test]
async fn failed_run_replaces_nothing() {
    let w = world().await;
    let id = w.create(w.website("Help centre")).await;
    let a = doc_id();
    w.put_doc(rag_doc(&a, w.sid, &id, "Returns", None));
    let first = w.completed_run(&id, &[&a]).await;
    let (_, run) = w.resync(&id, Value::Null).await;
    let rag_before = w.fake.rag_requests().len();
    w.settle(run["ingest_job_ids"][0].as_str().unwrap(), "failed", &[]);
    let now = w.runs(&id).await["items"][0].clone();
    assert_eq!(now["status"], "failed", "{now}");
    assert_eq!(now["replace_report"], Value::Null);
    assert!(w.has_doc(&a));
    assert!(
        w.fake.rag_requests()[rag_before..]
            .iter()
            .all(|r| r.method != "DELETE"),
        "a failed run deletes nothing"
    );
    assert_eq!(
        w.scalar_i64(
            "SELECT count(*) FROM source_run WHERE id = $1::uuid AND NOT documents_removed",
            first["id"].as_str().unwrap()
        )
        .await,
        1
    );
}

// ---- run detail ---------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn run_detail_lists_ingest_jobs_and_documents() {
    let w = world().await;
    let id = w.create(w.website("Help centre")).await;
    let (a, b, other) = (doc_id(), doc_id(), doc_id());
    w.put_doc(rag_doc(&b, w.sid, &id, "Shipping", None));
    w.put_doc(rag_doc(&a, w.sid, &id, "Returns", None));
    w.put_doc(rag_doc(
        &other,
        w.sid,
        &Uuid::new_v4().to_string(),
        "Other",
        None,
    ));
    let run = w.completed_run(&id, &[&a, &b]).await;
    let rid = run["id"].as_str().unwrap();
    let (s, d) = w.read(&src_uri(JOB, &format!("/{id}/runs/{rid}"))).await;
    assert_eq!(s, StatusCode::OK, "{d}");
    assert_eq!(d["run"]["id"], run["id"]);
    let jobs = d["ingest_jobs"].as_array().unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0]["id"], run["ingest_job_ids"][0]);
    assert_eq!(
        jobs[0]["store_id"],
        json!(w.leaf),
        "the agent store, never the strata id"
    );
    assert_eq!(jobs[0]["status"], "completed");
    let docs = d["documents"].as_array().unwrap();
    let titles: Vec<&str> = docs.iter().map(|x| x["title"].as_str().unwrap()).collect();
    assert_eq!(
        titles,
        ["Returns", "Shipping"],
        "this run's documents, by title"
    );
    assert_eq!(docs[0]["id"], json!(a));
    assert_eq!(docs[0]["store_id"], json!(w.leaf));
    assert_eq!(docs[0]["language"], "nl", "the source's language (D68)");
    assert_eq!(docs[0]["ingest_job_id"], run["ingest_job_ids"][0]);
    assert_eq!(docs[0]["tokens"], 100);
    let q = &w.rag_calls("GET", &format!("/api/stores/{}/documents", w.sid))[0];
    assert!(q.query.contains(&format!("source_id={id}")), "{}", q.query);
}

#[tokio::test]
async fn run_other_job_404() {
    let w = world().await;
    add_job(&w.e, w.tenant, "sales").await;
    let mine = w.create(w.website("Help centre")).await;
    let theirs = w.create_in("sales", w.website("Help centre")).await;
    let (s, run) = w
        .call(
            &w.km,
            "POST",
            &src_uri("sales", &format!("/{theirs}/resync")),
            Value::Null,
        )
        .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{run}");
    let rid = run["id"].as_str().unwrap();
    // another job's run under this job's source
    let (s, b) = w.read(&src_uri(JOB, &format!("/{mine}/runs/{rid}"))).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "run_not_found"),
        "{b}"
    );
    // another job's source under this job
    let (s, b) = w.read(&src_uri(JOB, &format!("/{theirs}/runs"))).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "source_not_found"),
        "{b}"
    );
    let (s, b) = w
        .read(&src_uri(JOB, &format!("/{theirs}/runs/{rid}")))
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "source_not_found"),
        "{b}"
    );
    let (s, b) = w
        .read(&src_uri(JOB, &format!("/{mine}/runs/{}", Uuid::new_v4())))
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "run_not_found"),
        "{b}"
    );
    // in its own job it is there
    let (s, b) = w
        .read(&src_uri("sales", &format!("/{theirs}/runs/{rid}")))
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

// ---- T6 carry-over: a failed feed job is re-run from its source ---------------------------------------------------------

#[tokio::test]
async fn retry_datatable_job_starts_new_source_run() {
    let w = world().await;
    let id = w.create(w.datatable("Products")).await;
    let (_, run) = w.resync(&id, Value::Null).await;
    let old = run["ingest_job_ids"][0].as_str().unwrap().to_string();
    w.settle(&old, "failed", &[]);
    let (s, view) = w
        .call(
            &w.km,
            "POST",
            &format!("/api/knowledge/ingest-jobs/{old}/retry"),
            Value::Null,
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{view}");
    assert_ne!(view["id"], json!(old), "the new job, not the old one");
    assert_eq!(view["status"], "queued");
    assert_eq!(view["source_id"], json!(id));
    assert!(
        w.fake
            .ingest_requests()
            .iter()
            .all(|r| !r.path.ends_with("/retry")),
        "strata's own retry is never asked (it keeps no credentials)"
    );
    let creates = w.ingest_creates();
    assert_eq!(creates.len(), 2);
    assert_eq!(
        creates[1].body["source"]["credentials"],
        json!({"secret": KEY})
    );
    let runs = w.runs(&id).await;
    let items = runs["items"].as_array().unwrap();
    assert_eq!(items.len(), 2, "a new source run");
    assert_eq!(items[0]["ingest_job_ids"][0], view["id"]);
    assert_eq!(
        items[1]["status"], "failed",
        "the old run took its job's outcome"
    );
    let audit: Value = sqlx::query_scalar(
        "SELECT details FROM audit_event WHERE action = 'ingest.retried' AND resource_id = $1",
    )
    .bind(&old)
    .fetch_one(&w.e.owner)
    .await
    .unwrap();
    assert_eq!(audit["resubmitted_as"], view["id"]);
    assert_eq!(audit["run_id"], items[0]["id"]);
    let trig: Value = sqlx::query_scalar(
        "SELECT details FROM audit_event WHERE action = 'source.resync' AND resource_id = $1 ORDER BY id DESC LIMIT 1",
    )
    .bind(&id)
    .fetch_one(&w.e.owner)
    .await
    .unwrap();
    assert_eq!(trig["trigger"], "retry");
    assert!(!w.stored_text().await.contains(KEY));
    // a failed feed job whose source is gone
    let orphan = Uuid::new_v4();
    lock(&w.fake.ingest).jobs.push((
        w.tenant,
        json!({"id": orphan, "store_id": w.sid, "status": "failed", "document_ids": [], "steps": [],
               "source": {"kind": "api", "config": {"url": "https://api.example.com/x"}},
               "meta": {"source_id": Uuid::new_v4().to_string()}, "error": null}),
    ));
    let (s, b) = w
        .call(
            &w.km,
            "POST",
            &format!("/api/knowledge/ingest-jobs/{orphan}/retry"),
            Value::Null,
        )
        .await;
    assert_eq!((s, code(&b)), (StatusCode::CONFLICT, "source_gone"), "{b}");
}

// ---- T7 carry-over: a store a source writes into ------------------------------------------------------------------------

#[tokio::test]
async fn delete_store_with_source_409_store_has_sources() {
    let w = world().await;
    let unbound = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO knowledge_store (tenant_id, id, key, label, kind, region, updated_by) \
         VALUES ($1, $2, 'faq.old', 'Old', 'leaf', 'eu', 'test')",
    )
    .bind(w.tenant)
    .bind(unbound)
    .execute(&w.e.owner)
    .await
    .unwrap();
    let mut body = w.website("Help centre");
    body["target_store_id"] = json!(unbound);
    let id = w.create(body).await;
    let (s, b) = w
        .call(
            &w.km,
            "DELETE",
            &format!("/api/knowledge/stores/{unbound}"),
            Value::Null,
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "store_has_sources"),
        "{b}"
    );
    assert_eq!(b["error"]["sources"], 1);
    assert_eq!(
        w.scalar_i64(
            "SELECT count(*) FROM knowledge_store WHERE id = $1::uuid",
            &unbound.to_string()
        )
        .await,
        1,
        "the store stays"
    );
    let (s, b) = w
        .call(
            &w.km,
            "DELETE",
            &src_uri(JOB, &format!("/{id}")),
            Value::Null,
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = w
        .call(
            &w.km,
            "DELETE",
            &format!("/api/knowledge/stores/{unbound}"),
            Value::Null,
        )
        .await;
    assert_eq!(s, StatusCode::NO_CONTENT, "{b}");
}
