//! B8 T12 + T12a — the validation grid per release cut, question hygiene (F-50) and the grid run on approve (F-06,
//! B8-9). Real Postgres 18, a job created through `POST /api/jobs`, through the HTTP router as `agent_app`. Grading
//! itself (citation check before the judge, Laya down → pending, the budget) is unit-tested in
//! `validation::judge`; here no strata / Laya is configured, so a run grades nothing (503 / grid `failed`) and the
//! rows a test needs graded are written by the owner pool.
use std::time::Duration;

use agent_api::validation::grid;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::common::design_contract::{self, ContractJob};
use crate::common::{self, TestEnv, get, insert_user, person, send};

const ADMIN: &str = "u-val-admin";
const DESIGNER: &str = "u-val-designer";
const OBSERVER: &str = "u-val-observer";
const BOUNDARY: &str = "b8-validation-boundary";

struct Fx {
    e: TestEnv,
    f: ContractJob,
    admin: String,
    designer: String,
    observer: String,
}

async fn setup() -> Fx {
    let e = common::env().await;
    let f = design_contract::new_job(&e).await;
    insert_user(&e.owner, ADMIN, f.tenant, "tenant_admin").await;
    insert_user(&e.owner, DESIGNER, f.tenant, "job_designer").await;
    insert_user(&e.owner, OBSERVER, f.tenant, "").await;
    Fx {
        e,
        f,
        admin: person(ADMIN, &[]),
        designer: person(DESIGNER, &[]),
        observer: person(OBSERVER, &[]),
    }
}

fn uri(x: &Fx, tail: &str) -> String {
    format!("/api/jobs/{}/{tail}", x.f.job.job_key)
}

fn code(b: &Value) -> &str {
    b["error"]["code"].as_str().unwrap_or("")
}

async fn call(x: &Fx, method: &str, tail: &str, tok: &str, body: Value) -> (StatusCode, Value) {
    send(
        &x.e.app,
        common::json(method, &uri(x, tail), Some(tok), body),
    )
    .await
}

async fn read(x: &Fx, tail: &str, tok: &str) -> Value {
    let (s, b) = send(&x.e.app, get(&uri(x, tail), Some(tok))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    b
}

async fn add(x: &Fx, q: &str) -> Value {
    let (s, b) = call(
        x,
        "POST",
        "validation/questions",
        &x.designer,
        json!({"question": q, "expected": "30 days", "reference": "policy.returns §2"}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    b
}

async fn question_id(x: &Fx, q: &str) -> Uuid {
    sqlx::query_scalar("SELECT id FROM validation_question WHERE job_id = $1 AND question = $2")
        .bind(x.f.job.job_id)
        .bind(q)
        .fetch_one(&x.e.owner)
        .await
        .unwrap()
}

/// A judged result written as the judge would have (owner pool).
async fn judged(x: &Fx, release: &str, q: Uuid, verdict: &str, reason: Option<&str>) {
    sqlx::query(
        "INSERT INTO validation_result (tenant_id, job_id, release_id, question_id, response, cited, verdict, \
                                        reason, judged_by, judged_at) \
         VALUES ($1, $2, $3, $4, 'You have 30 days.', '{policy.returns}', $5, $6, 'judge', now())",
    )
    .bind(x.f.tenant)
    .bind(x.f.job.job_id)
    .bind(release)
    .bind(q)
    .bind(verdict)
    .bind(reason)
    .execute(&x.e.owner)
    .await
    .unwrap();
}

async fn set_state(x: &Fx, release: &str, state: &str) {
    sqlx::query("UPDATE job_release SET state = $3 WHERE job_id = $1 AND release_id = $2")
        .bind(x.f.job.job_id)
        .bind(release)
        .bind(state)
        .execute(&x.e.owner)
        .await
        .unwrap();
}

fn cut_of<'a>(page: &'a Value, release: &str) -> &'a Value {
    page["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["release"]["id"] == release)
        .unwrap_or_else(|| panic!("no cut {release}: {page}"))
}

fn questions_of(cut: &Value) -> Vec<String> {
    cut["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["question"].as_str().unwrap().to_string())
        .collect()
}

fn multipart(file_name: &str, content: &str) -> Body {
    let ty = if file_name.ends_with(".csv") {
        "text/csv"
    } else {
        "text/plain"
    };
    Body::from(format!(
        "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{file_name}\"\r\n\
         Content-Type: {ty}\r\n\r\n{content}\r\n--{BOUNDARY}--\r\n"
    ))
}

async fn import(x: &Fx, file_name: &str, content: &str) -> (StatusCode, Value) {
    let req = Request::post(uri(x, "validation/questions/import"))
        .header("authorization", format!("Bearer {}", x.designer))
        .header(
            "content-type",
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(multipart(file_name, content))
        .unwrap();
    send(&x.e.app, req).await
}

async fn grid_rows(x: &Fx) -> Vec<(String, String, String)> {
    sqlx::query_as(
        "SELECT release_id, trigger, status FROM validation_grid_run WHERE job_id = $1 ORDER BY queued_at, id",
    )
    .bind(x.f.job.job_id)
    .fetch_all(&x.e.owner)
    .await
    .unwrap()
}

/// Wait until the job has at least `rows` grid runs and none is active. The approve hook inserts its row on a task
/// spawned after the approve's response, so "nothing active" alone is also true before that row exists.
async fn settle(x: &Fx, rows: usize) {
    for _ in 0..200 {
        let r = grid_rows(x).await;
        if r.len() >= rows && r.iter().all(|(_, _, s)| s != "queued" && s != "running") {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!(
        "expected {rows} settled grid runs: {:?}",
        grid_rows(x).await
    );
}

#[tokio::test]
async fn tenant_user_reads_cannot_run() {
    let x = setup().await;
    let rel = design_contract::publish(&x.e, &x.f).await;
    let page = read(&x, "validation", &x.observer).await;
    assert_eq!(cut_of(&page, &rel)["state"], "current");
    let (s, b) = call(
        &x,
        "POST",
        &format!("validation/{rel}/run"),
        &x.observer,
        json!({}),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
    assert_eq!(b["error"]["missing_role"], "release_manager", "{b}");
    let (s, b) = call(
        &x,
        "POST",
        "validation/questions",
        &x.observer,
        json!({"question": "q", "expected": "e", "reference": "r"}),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
    assert_eq!(b["error"]["missing_role"], "job_designer", "{b}");
}

#[tokio::test]
async fn add_question_needs_release_and_fields() {
    let x = setup().await;
    let body =
        json!({"question": "How long?", "expected": "30 days", "reference": "policy.returns"});
    let (s, b) = call(
        &x,
        "POST",
        "validation/questions",
        &x.designer,
        body.clone(),
    )
    .await;
    assert_eq!((s, code(&b)), (StatusCode::CONFLICT, "no_release"), "{b}");
    let (s, b) = import(&x, "q.csv", "question,expected,reference\nq,e,r\n").await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "no_release"),
        "import_needs_release: {b}"
    );
    design_contract::publish(&x.e, &x.f).await;
    let (s, b) = call(
        &x,
        "POST",
        "validation/questions",
        &x.designer,
        json!({"question": " ", "expected": "e", "reference": "r"}),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "fields_required"),
        "{b}"
    );
    let (s, b) = call(&x, "POST", "validation/questions", &x.designer, body).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(b["items"][0]["rows"][0]["verdict"], "pending", "{b}");
    assert_eq!(b["items"][0]["rows"][0]["response"], Value::Null, "{b}");
}

#[tokio::test]
async fn question_applies_to_current_and_approved_only() {
    let x = setup().await;
    let rel = design_contract::publish(&x.e, &x.f).await;
    add(&x, "How long do I have?").await;
    let page = read(&x, "validation", &x.observer).await;
    assert_eq!(questions_of(cut_of(&page, &rel)), ["How long do I have?"]);
    // an applied_earlier cut keeps only its stored results: none here
    set_state(&x, &rel, "applied_earlier").await;
    let page = read(&x, "validation", &x.observer).await;
    assert!(questions_of(cut_of(&page, &rel)).is_empty(), "{page}");
}

#[tokio::test]
async fn deleted_question_leaves_current_cut_keeps_old_results() {
    let x = setup().await;
    let rel = design_contract::publish(&x.e, &x.f).await;
    add(&x, "Old question").await;
    add(&x, "Kept question").await;
    let old = question_id(&x, "Old question").await;
    judged(&x, &rel, old, "pass", None).await;
    let (s, b) = call(
        &x,
        "DELETE",
        &format!("validation/questions/{old}"),
        &x.designer,
        json!({}),
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT, "{b}");
    let (s, b) = call(
        &x,
        "DELETE",
        &format!("validation/questions/{old}"),
        &x.designer,
        json!({}),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "question_not_found"),
        "{b}"
    );
    let page = read(&x, "validation", &x.observer).await;
    assert_eq!(questions_of(cut_of(&page, &rel)), ["Kept question"]);
    set_state(&x, &rel, "applied_earlier").await;
    let page = read(&x, "validation", &x.observer).await;
    let cut = cut_of(&page, &rel);
    assert_eq!(
        questions_of(cut),
        ["Old question"],
        "the stored result keeps its question"
    );
    assert_eq!(cut["rows"][0]["verdict"], "pass");
}

#[tokio::test]
async fn human_verdict_overrides_judge() {
    let x = setup().await;
    let rel = design_contract::publish(&x.e, &x.f).await;
    add(&x, "Judged").await;
    add(&x, "Never run").await;
    let judged_q = question_id(&x, "Judged").await;
    let never = question_id(&x, "Never run").await;
    judged(&x, &rel, judged_q, "rejected", Some("incorrect")).await;
    let row = |q: Uuid| format!("validation/{rel}/rows/{q}");
    let (s, b) = call(&x, "PUT", &row(never), &x.admin, json!({"verdict": "pass"})).await;
    assert_eq!((s, code(&b)), (StatusCode::CONFLICT, "not_run"), "{b}");
    let (s, b) = call(
        &x,
        "PUT",
        &row(judged_q),
        &x.admin,
        json!({"verdict": "rejected"}),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "reason_required"),
        "{b}"
    );
    let (s, b) = call(
        &x,
        "PUT",
        &row(Uuid::new_v4()),
        &x.admin,
        json!({"verdict": "pass"}),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "row_not_found"),
        "{b}"
    );
    let (s, b) = call(
        &x,
        "PUT",
        &format!("validation/cfg-2000-01-01.1/rows/{judged_q}"),
        &x.admin,
        json!({"verdict": "pass"}),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "release_not_found"),
        "{b}"
    );
    let (s, b) = call(
        &x,
        "PUT",
        &row(judged_q),
        &x.admin,
        json!({"verdict": "pass"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let r = b["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["question"] == "Judged")
        .unwrap();
    assert_eq!(r["verdict"], "pass");
    assert_eq!(r["reason"], Value::Null);
    assert_eq!(r["judged_by"], ADMIN);
    // a later judge write never overwrites a person's verdict
    let n = sqlx::query(
        "UPDATE validation_result SET verdict = 'rejected', reason = 'incorrect' \
          WHERE question_id = $1 AND (judged_by IS NULL OR judged_by = 'judge')",
    )
    .bind(judged_q)
    .execute(&x.e.owner)
    .await
    .unwrap()
    .rows_affected();
    assert_eq!(n, 0);
}

#[tokio::test]
async fn update_question_validates_fields() {
    let x = setup().await;
    design_contract::publish(&x.e, &x.f).await;
    add(&x, "Before").await;
    let id = question_id(&x, "Before").await;
    let path = format!("validation/questions/{id}");
    let (s, b) = call(&x, "PATCH", &path, &x.designer, json!({"expected": "  "})).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "fields_required"),
        "{b}"
    );
    let (s, b) = call(
        &x,
        "PATCH",
        &path,
        &x.designer,
        json!({"question": " After "}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["items"][0]["rows"][0]["question"], "After", "{b}");
    let (s, b) = call(
        &x,
        "PATCH",
        &format!("validation/questions/{}", Uuid::new_v4()),
        &x.designer,
        json!({"question": "x"}),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "question_not_found"),
        "{b}"
    );
}

#[tokio::test]
async fn import_bad_row_rejects_whole_file() {
    let x = setup().await;
    design_contract::publish(&x.e, &x.f).await;
    let (s, b) = import(&x, "q.csv", "question,expected,reference\nq1,e,r\n,e,r\n").await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_rows"),
        "{b}"
    );
    assert_eq!(b["error"]["rows"], json!([3]), "{b}");
    let (s, b) = import(&x, "q.txt", "hello").await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "unsupported_file"),
        "{b}"
    );
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM validation_question WHERE job_id = $1")
        .bind(x.f.job.job_id)
        .fetch_one(&x.e.owner)
        .await
        .unwrap();
    assert_eq!(n, 0, "nothing imported");
    let (s, b) = import(
        &x,
        "q.csv",
        "question,expected,reference,activity\nq1,e,r,after_sales:return_item\nq2,e,r,\n",
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(b["imported"], 2);
    let sources: Vec<(String, Option<String>)> =
        sqlx::query_as("SELECT source, import_batch FROM validation_question WHERE job_id = $1")
            .bind(x.f.job.job_id)
            .fetch_all(&x.e.owner)
            .await
            .unwrap();
    assert!(
        sources
            .iter()
            .all(|(s, batch)| s == "csv" && batch.as_deref() == b["import_batch"].as_str())
    );
}

#[tokio::test]
async fn import_500_ok_501_413() {
    let x = setup().await;
    design_contract::publish(&x.e, &x.f).await;
    let mut csv = String::from("question,expected,reference\n");
    for i in 0..499 {
        csv.push_str(&format!("q{i},e,r\n"));
    }
    let (s, b) = import(&x, "q.csv", &csv).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    add(&x, "the 500th").await;
    let (s, b) = import(&x, "q.csv", "question,expected,reference\none,more,row\n").await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::PAYLOAD_TOO_LARGE, "too_many_questions"),
        "{b}"
    );
}

#[tokio::test]
async fn manual_run_while_grid_running_409_and_no_questions() {
    let x = setup().await;
    let rel = design_contract::publish(&x.e, &x.f).await;
    let run = format!("validation/{rel}/run");
    let (s, b) = call(&x, "POST", &run, &x.admin, json!({})).await;
    assert_eq!((s, code(&b)), (StatusCode::CONFLICT, "no_questions"), "{b}");
    add(&x, "Graded later").await;
    sqlx::query(
        "INSERT INTO validation_grid_run (tenant_id, job_id, release_id, trigger, status, total, by) \
         VALUES ($1, $2, $3, 'approve', 'running', 1, $4)",
    )
    .bind(x.f.tenant)
    .bind(x.f.job.job_id)
    .bind(&rel)
    .bind(ADMIN)
    .execute(&x.e.owner)
    .await
    .unwrap();
    let (s, b) = call(&x, "POST", &run, &x.admin, json!({})).await;
    assert_eq!((s, code(&b)), (StatusCode::CONFLICT, "grid_running"), "{b}");
    let (s, b) = call(
        &x,
        "POST",
        "validation/cfg-2000-01-01.1/run",
        &x.admin,
        json!({}),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "release_not_found"),
        "{b}"
    );
}

#[tokio::test]
async fn no_questions_is_skipped_and_approve_queues_one_grid_run_after_commit() {
    let x = setup().await;
    let _boot = grid::install(&x.e.state);
    let first = design_contract::publish(&x.e, &x.f).await;
    settle(&x, 1).await;
    assert_eq!(
        grid_rows(&x).await,
        [(
            first.clone(),
            "approve".into(),
            "skipped_no_questions".into()
        )],
        "no live question: skipped, no task"
    );
    add(&x, "Graded on approve").await;
    let (s, b) = call(
        &x,
        "PUT",
        "languages",
        &x.admin,
        json!({"languages": ["en", "de"]}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    // grid_failure_does_not_fail_approve: no strata is configured here, so the grid cannot answer
    let next = design_contract::cut(&x.e, &x.f, &x.admin, false).await;
    settle(&x, 2).await;
    let rows = grid_rows(&x).await;
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(
        (rows[1].0.as_str(), rows[1].1.as_str()),
        (next.as_str(), "approve")
    );
    assert!(rows[1].2 == "done" || rows[1].2 == "failed", "{rows:?}");
    let state: String =
        sqlx::query_scalar("SELECT state FROM job_release WHERE job_id = $1 AND release_id = $2")
            .bind(x.f.job.job_id)
            .bind(&next)
            .fetch_one(&x.e.owner)
            .await
            .unwrap();
    assert_eq!(state, "approved", "the approve stands");
    // apply_succeeds_while_grid_running: an active grid run never blocks apply
    sqlx::query(
        "INSERT INTO validation_grid_run (tenant_id, job_id, release_id, trigger, status, total, by) \
         VALUES ($1, $2, $3, 'manual', 'running', 1, $4)",
    )
    .bind(x.f.tenant)
    .bind(x.f.job.job_id)
    .bind(&next)
    .bind(ADMIN)
    .execute(&x.e.owner)
    .await
    .unwrap();
    let (s, b) = call(
        &x,
        "POST",
        &format!("releases/{next}/apply"),
        &x.admin,
        json!({}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    // release_validation_shows_status_and_rows
    let v = read(&x, &format!("releases/{next}/validation"), &x.observer).await;
    assert_eq!(v["release"]["id"], next.as_str(), "{v}");
    assert_eq!(questions_of(&v), ["Graded on approve"], "{v}");
    assert_eq!(v["grid"]["trigger"], "manual", "the latest run: {v}");
    assert_eq!(v["grid"]["status"], "running", "{v}");
    let audited: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_event WHERE action = 'validation.grid_queued' AND job = $1",
    )
    .bind(&x.f.job.job_key)
    .fetch_one(&x.e.owner)
    .await
    .unwrap();
    assert_eq!(audited, 2);
}

#[tokio::test]
async fn grid_never_started_without_an_approve_and_boot_resumes_queued_grid() {
    let x = setup().await;
    let rel = design_contract::publish(&x.e, &x.f).await;
    assert_eq!(
        grid::resume(&x.e.state).await.unwrap(),
        0,
        "nothing queued: nothing starts"
    );
    assert!(
        grid_rows(&x).await.is_empty(),
        "no hook installed, no approve-less run"
    );
    add(&x, "Resumed").await;
    sqlx::query(
        "INSERT INTO validation_grid_run (tenant_id, job_id, release_id, trigger, status, total, by) \
         VALUES ($1, $2, $3, 'approve', 'queued', 1, $4)",
    )
    .bind(x.f.tenant)
    .bind(x.f.job.job_id)
    .bind(&rel)
    .bind(ADMIN)
    .execute(&x.e.owner)
    .await
    .unwrap();
    assert_eq!(grid::resume(&x.e.state).await.unwrap(), 1);
    settle(&x, 1).await;
    let rows = grid_rows(&x).await;
    assert!(rows[0].2 == "done" || rows[0].2 == "failed", "{rows:?}");
}
