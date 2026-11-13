//! B8 T14a — discovery import and parsing (plan § Revision 2026-10-22 task split): upload (multipart), CSV / JSON
//! parse, PII scrub + drop, dedupe with weight, the worker over a fake classifier, boot resume, tenant isolation.
//! Real Postgres 18, a Demo Retail job created through `POST /api/jobs`, through the HTTP router as `agent_app`.
//! Classification proper (Laya, thresholds, clusters) is T14b, after KH.
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use agent_api::discovery::worker::{
    self, Bucket, Classified, Classifier, ImportRef, Outcome, Pending,
};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};

use crate::common::design_contract::{self, ContractJob, DESIGNER};
use crate::common::{self, TestEnv, get, insert_user, person, send};

pub const BOUNDARY: &str = "b8-discovery-boundary";

pub struct Fx {
    pub e: TestEnv,
    pub f: ContractJob,
    /// `job_designer` of the job's tenant.
    pub designer: String,
}

pub async fn setup() -> Fx {
    let e = common::env().await;
    let f = crate::b8::support::new_job(&e).await;
    insert_user(&e.owner, DESIGNER, f.tenant, "job_designer").await;
    Fx {
        e,
        f,
        designer: person(DESIGNER, &[]),
    }
}

/// A second tenant's job and its designer (another sub) on the same database.
pub async fn other(x: &Fx) -> (ContractJob, String) {
    let f = design_contract::new_job(&x.e).await;
    insert_user(&x.e.owner, "u-designer-other", f.tenant, "job_designer").await;
    (f, person("u-designer-other", &[]))
}

pub fn uri(f: &ContractJob, tail: &str) -> String {
    format!("/api/jobs/{}/discovery{tail}", f.job.job_key)
}

/// A multipart body: the text `fields`, then `file` as (name, bytes).
pub fn form(file: (&str, &[u8]), fields: &[(&str, &str)]) -> Vec<u8> {
    let mut b: Vec<u8> = Vec::new();
    for (k, v) in fields {
        b.extend_from_slice(
            format!("--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n")
                .as_bytes(),
        );
    }
    b.extend_from_slice(
        format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{}\"\r\n\
             Content-Type: application/octet-stream\r\n\r\n",
            file.0
        )
        .as_bytes(),
    );
    b.extend_from_slice(file.1);
    b.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
    b
}

pub async fn post_form(
    x: &Fx,
    f: &ContractJob,
    token: &str,
    tail: &str,
    body: Vec<u8>,
) -> (StatusCode, Value) {
    let req = Request::post(uri(f, tail))
        .header("authorization", format!("Bearer {token}"))
        .header(
            "content-type",
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(Body::from(body))
        .unwrap();
    send(&x.e.app, req).await
}

/// Upload `file` to the fixture job as its designer.
pub async fn upload(
    x: &Fx,
    file: &str,
    bytes: &[u8],
    fields: &[(&str, &str)],
) -> (StatusCode, Value) {
    post_form(
        x,
        &x.f,
        &x.designer,
        "/imports",
        form((file, bytes), fields),
    )
    .await
}

#[track_caller]
pub fn ok(got: (StatusCode, Value)) -> Value {
    assert_eq!(got.0, StatusCode::OK, "{}", got.1);
    got.1
}

#[track_caller]
pub fn refused(got: (StatusCode, Value), status: StatusCode, code: &str) {
    assert_eq!(got.0, status, "{}", got.1);
    assert_eq!(got.1["error"]["code"], json!(code), "{}", got.1);
}

/// The id of the latest import in a `DiscoveryState`.
pub fn latest(state: &Value) -> String {
    state["imports"][0]["id"].as_str().unwrap().to_string()
}

pub fn import_ref(f: &ContractJob, id: &str) -> ImportRef {
    ImportRef {
        tenant: f.tenant,
        job_id: f.job.job_id,
        id: id.to_string(),
    }
}

/// `(text_hash, excerpt, weight)` of an import's rows, by first occurrence (owner pool: RLS does not apply).
pub async fn stored_rows(x: &Fx, id: &str) -> Vec<(String, Option<String>, i32)> {
    sqlx::query_as(
        "SELECT text_hash, excerpt, weight FROM discovery_row WHERE import_id = $1 ORDER BY ord",
    )
    .bind(id)
    .fetch_all(&x.e.owner)
    .await
    .unwrap()
}

/// A fake classifier: an excerpt with `refund` → hidden `billing:refund`, `b8` → out of scope (no excerpt kept),
/// `skip` → no answer (fails open to unclear), anything else → covered `delivery:where_is_my_order`. Counts the rows it saw.
#[derive(Default)]
pub struct Fake {
    pub seen: AtomicUsize,
    pub calls: AtomicUsize,
    pub hashes: Mutex<Vec<String>>,
}

impl Classifier for Fake {
    fn classify<'a>(
        &'a self,
        _at: &'a ImportRef,
        rows: &'a [Pending],
    ) -> BoxFuture<'a, Option<Vec<Classified>>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.seen.fetch_add(rows.len(), Ordering::SeqCst);
        self.hashes
            .lock()
            .unwrap()
            .extend(rows.iter().map(|r| r.text_hash.clone()));
        let out = rows
            .iter()
            .filter_map(|r| {
                let e = r.excerpt.clone().unwrap_or_default().to_lowercase();
                if e.contains("skip") {
                    return None;
                }
                let (bucket, stage, activity, keep) = if e.contains("refund") {
                    (
                        Bucket::Hidden,
                        Some("billing"),
                        Some("billing:refund"),
                        true,
                    )
                } else if e.contains("b8") {
                    (Bucket::OutOfScope, None, None, false)
                } else {
                    (
                        Bucket::Covered,
                        Some("delivery"),
                        Some("delivery:where_is_my_order"),
                        true,
                    )
                };
                Some(Classified {
                    text_hash: r.text_hash.clone(),
                    bucket,
                    stage: stage.map(str::to_string),
                    activity: activity.map(str::to_string),
                    confidence: Some(0.9),
                    keep_excerpt: keep,
                })
            })
            .collect();
        Box::pin(async move { Some(out) })
    }
}

/// Upload `csv` and classify it to the end with [`Fake`]; returns the import id.
pub async fn classified(x: &Fx, csv: &str) -> String {
    let id = latest(&ok(upload(x, "tickets.csv", csv.as_bytes(), &[]).await));
    let fake = Fake::default();
    let out = worker::run(&x.e.state.db, &fake, &import_ref(&x.f, &id)).await;
    assert_ran_to_classified(x, &id, out).await;
    id
}

/// Wait until no worker holds `id`'s lease (the upload's own unwired worker claims, parks and releases within
/// milliseconds; the pause lets it claim first).
pub async fn wait_unleased(x: &Fx, id: &str) {
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    for _ in 0..200 {
        let owner: Option<String> =
            sqlx::query_scalar("SELECT lease_owner FROM discovery_import WHERE id = $1")
                .bind(id)
                .fetch_one(&x.e.owner)
                .await
                .unwrap();
        if owner.is_none() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("import {id} is still leased");
}

/// A worker run over `id` that reached the end. The upload's own (unwired) worker runs concurrently: when it starts
/// after this run bucketed the last batch it finds nothing pending and finishes the import itself, and this run then
/// stops as `Cancelled`. Either way the import must be `classified`.
pub async fn assert_ran_to_classified(x: &Fx, id: &str, out: Outcome) {
    assert!(
        matches!(out, Outcome::Classified | Outcome::Cancelled),
        "{out:?}"
    );
    let status: String = sqlx::query_scalar("SELECT status FROM discovery_import WHERE id = $1")
        .bind(id)
        .fetch_one(&x.e.owner)
        .await
        .unwrap();
    assert_eq!(status, "classified");
}

#[tokio::test]
async fn upload_answers_state_with_a_classifying_import() {
    let x = setup().await;
    let v = ok(upload(
        &x,
        "zendesk.csv",
        b"id,subject\n1,where is my order\n2,refund please\n",
        &[],
    )
    .await);
    let imp = &v["imports"][0];
    assert_eq!(imp["status"], "classifying", "{v}");
    assert_eq!(imp["file"], "zendesk.csv");
    assert_eq!(imp["rows"], 2);
    assert_eq!(imp["distinct_rows"], 2);
    assert_eq!(imp["progress"], 0);
    assert_eq!(imp["coverage"], Value::Null);
    assert_eq!(imp["format"], "csv");
    assert_eq!(imp["text_column"], "subject");
    assert!(imp["id"].as_str().unwrap().starts_with("imp-"));
    assert_eq!(v["shadow"]["active"], false);
    assert_eq!(v["runs"], json!([]));
    // a member reads the same state
    let (s, read) = send(&x.e.app, get(&uri(&x.f, ""), Some(&x.designer))).await;
    assert_eq!(s, StatusCode::OK, "{read}");
    assert_eq!(read["imports"][0]["id"], imp["id"]);
    // the state names the job's industry and primary department (D115), not null
    assert!(
        read["industry"].as_str().is_some_and(|i| !i.is_empty()),
        "{read}"
    );
    assert!(
        read["department"].as_str().is_some_and(|d| !d.is_empty()),
        "{read}"
    );
    let audit: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_event WHERE action = 'discovery.import_upload' AND resource_id = $1",
    )
    .bind(imp["id"].as_str().unwrap())
    .fetch_one(&x.e.owner)
    .await
    .unwrap();
    assert_eq!(audit, 1);
}

#[tokio::test]
async fn card_number_row_is_never_stored() {
    let x = setup().await;
    let v = ok(upload(
        &x,
        "t.csv",
        b"text\nmy card 4111 1111 1111 1111 was charged twice\nmy password is hunter2\nwhere is my order\n",
        &[],
    )
    .await);
    let id = latest(&v);
    let rows = stored_rows(&x, &id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].1.as_deref(), Some("where is my order"));
    let leaked: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM discovery_row WHERE excerpt LIKE '%4111%' OR excerpt LIKE '%hunter2%'",
    )
    .fetch_one(&x.e.owner)
    .await
    .unwrap();
    assert_eq!(leaked, 0);
    assert_eq!(v["imports"][0]["rows"], 3);
    assert_eq!(v["imports"][0]["distinct_rows"], 1);
}

#[tokio::test]
async fn excerpt_is_scrubbed() {
    let x = setup().await;
    let v = ok(upload(
        &x,
        "t.json",
        br#"["Mail me at anna@example.com about order 1234567890"]"#,
        &[],
    )
    .await);
    let rows = stored_rows(&x, &latest(&v)).await;
    assert_eq!(
        rows[0].1.as_deref(),
        Some("Mail me at [email] about order [number]")
    );
    let long = format!("text\n{}\n", "ab ".repeat(1000));
    let v = ok(upload(&x, "long.csv", long.as_bytes(), &[]).await);
    let rows = stored_rows(&x, &latest(&v)).await;
    assert!(rows[0].1.as_ref().unwrap().len() <= 1024);
}

#[tokio::test]
async fn duplicates_classified_once_with_weight() {
    let x = setup().await;
    let id = latest(&ok(upload(
        &x,
        "t.csv",
        b"text\nWhere is my order\nwhere is  MY order\nwhere is my order\nrefund please\n",
        &[],
    )
    .await));
    let rows = stored_rows(&x, &id).await;
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].2, 3);
    let fake = Fake::default();
    let out = worker::run(&x.e.state.db, &fake, &import_ref(&x.f, &id)).await;
    assert_ran_to_classified(&x, &id, out).await;
    assert_eq!(
        fake.seen.load(Ordering::SeqCst),
        2,
        "each distinct row once"
    );
    let v = ok(send(&x.e.app, get(&uri(&x.f, ""), Some(&x.designer))).await);
    let imp = &v["imports"][0];
    assert_eq!(imp["status"], "classified", "{v}");
    assert_eq!(imp["progress"], 1);
    assert!(
        (imp["coverage"].as_f64().unwrap() - 0.75).abs() < 1e-6,
        "{imp}"
    );
    let vols = imp["volumes"].as_array().unwrap();
    assert_eq!(vols[0]["activity"], "delivery:where_is_my_order");
    assert_eq!(vols[0]["tickets"], 3);
    assert_eq!(vols[0]["enabled"], true);
    assert_eq!(vols[1]["activity"], "billing:refund");
    assert_eq!(vols[1]["enabled"], false);
}

#[tokio::test]
async fn classifier_silence_fails_open_to_unclear_and_b8_keeps_no_excerpt() {
    let x = setup().await;
    let id = classified(
        &x,
        "text\nplease skip me\nb8 question about weather\nwhere is my order\n",
    )
    .await;
    let got: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT bucket, excerpt FROM discovery_row WHERE import_id = $1 ORDER BY ord",
    )
    .bind(&id)
    .fetch_all(&x.e.owner)
    .await
    .unwrap();
    assert_eq!(got[0].0, "unclear");
    assert_eq!(got[1], ("out_of_scope".to_string(), None));
    assert_eq!(got[2].0, "covered");
    let v = ok(send(&x.e.app, get(&uri(&x.f, ""), Some(&x.designer))).await);
    assert_eq!(v["imports"][0]["long_tail"], 1);
}

#[tokio::test]
async fn json_and_csv_both_parse() {
    let x = setup().await;
    let v = ok(upload(
        &x,
        "a.json",
        br#"["where is my order", "refund please"]"#,
        &[],
    )
    .await);
    assert_eq!(v["imports"][0]["distinct_rows"], 2);
    assert_eq!(v["imports"][0]["format"], "json");
    let v = ok(upload(
        &x,
        "b.json",
        br#"[{"id": 1, "body": "where is my order"}, {"id": 2, "body": "refund"}]"#,
        &[],
    )
    .await);
    assert_eq!(v["imports"][0]["text_column"], "body");
    let v = ok(upload(&x, "c.csv", b"subject;id\nwhere is my order;1\n", &[]).await);
    assert_eq!(v["imports"][0]["text_column"], "subject");
    assert_eq!(v["imports"].as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn unsupported_and_empty_files_are_422() {
    let x = setup().await;
    refused(
        upload(&x, "t.pdf", b"%PDF", &[]).await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "unsupported_file",
    );
    refused(
        upload(&x, "t.json", br#"{"a": 1}"#, &[]).await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "unsupported_file",
    );
    refused(
        upload(&x, "t.csv", b"text\n", &[]).await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "empty_file",
    );
    refused(
        upload(&x, "t.csv", b"text\nmy password is hunter2\n", &[]).await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "empty_file",
    );
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM discovery_import")
        .fetch_one(&x.e.owner)
        .await
        .unwrap();
    assert_eq!(n, 0);
}

#[tokio::test]
async fn over_limit_is_413() {
    let x = setup().await;
    let mut csv = String::from("text\n");
    for i in 0..50_001 {
        csv.push_str(&format!("ticket {i}\n"));
    }
    refused(
        upload(&x, "big.csv", csv.as_bytes(), &[]).await,
        StatusCode::PAYLOAD_TOO_LARGE,
        "too_large",
    );
    let huge = vec![b'a'; 21 * 1024 * 1024];
    refused(
        upload(&x, "huge.csv", &huge, &[]).await,
        StatusCode::PAYLOAD_TOO_LARGE,
        "too_large",
    );
}

#[tokio::test]
async fn restart_resumes_classifying_import() {
    let x = setup().await;
    let id = latest(&ok(
        upload(&x, "t.csv", b"text\nwhere is my order\n", &[]).await
    ));
    // the upload's own worker has no classifier yet (T14a): the import stays classifying
    let out = worker::run(&x.e.state.db, &worker::Unwired, &import_ref(&x.f, &id)).await;
    assert_eq!(out, Outcome::Parked);
    // the upload's own worker leases the import while it runs: resume only lists what no live lease holds
    wait_unleased(&x, &id).await;
    let fake = Arc::new(Fake::default());
    let handles = worker::resume(&x.e.state.db, fake.clone()).await.unwrap();
    assert_eq!(handles.len(), 1);
    for h in handles {
        assert_ran_to_classified(&x, &id, h.await.unwrap()).await;
    }
    // nothing left to resume: boot creates no work
    assert!(
        worker::resume(&x.e.state.db, fake)
            .await
            .unwrap()
            .is_empty()
    );
}

/// A worker that finds its import already `classified` (a concurrent worker on the same import — the upload's own,
/// a boot resume, another replica — finished it between two of its batches) ends `Classified`, not `Cancelled`.
/// This was the race behind the flaky `classified()` helper (main red after #73).
#[tokio::test]
async fn worker_on_an_import_another_worker_finished_ends_classified() {
    let x = setup().await;
    let id = classified(&x, "text\nwhere is my order\nrefund please\n").await;
    let fake = Fake::default();
    let out = worker::run(&x.e.state.db, &fake, &import_ref(&x.f, &id)).await;
    assert_eq!(out, Outcome::Classified);
    // it classified nothing itself
    assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn other_tenant_cannot_see_import() {
    let x = setup().await;
    ok(upload(&x, "t.csv", b"text\nwhere is my order\n", &[]).await);
    let (g, token) = other(&x).await;
    let v = ok(send(&x.e.app, get(&uri(&g, ""), Some(&token))).await);
    assert_eq!(v["imports"], json!([]));
    // and an agent_app transaction scoped to the other tenant sees no row
    let mut tx = agent_api::tenancy::begin_scoped(&x.e.state.db, g.tenant)
        .await
        .unwrap();
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM discovery_row")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(n, 0);
}

#[tokio::test]
async fn an_operator_cannot_upload() {
    let x = setup().await;
    insert_user(&x.e.owner, "u-operator", x.f.tenant, "operator").await;
    let token = person("u-operator", &[]);
    let got = post_form(
        &x,
        &x.f,
        &token,
        "/imports",
        form(("t.csv", b"text\nhello\n"), &[]),
    )
    .await;
    refused(got, StatusCode::FORBIDDEN, "forbidden");
}
