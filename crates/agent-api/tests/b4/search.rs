//! Console follow-up: `GET /api/search?q=` — the palette's case hits across a tenant's jobs. Every tenant member
//! reads every job's cases (the per-job route's rule), so the hits span jobs and carry the job key; another
//! tenant's cases never appear; a short query is empty; no auth is 401. Real Postgres 18.
use axum::http::StatusCode;
use serde_json::Value;
use uuid::Uuid;

use crate::common::conv::{self, Ctx, code};
use crate::common::{get, insert_tenant, send};

async fn job(c: &Ctx, tenant: Uuid, key: &str) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO jobs (tenant_id, key, label, pins, created_by) VALUES ($1, $2, $3, '[]', 'test') RETURNING id",
    )
    .bind(tenant)
    .bind(key)
    .bind(format!("Job {key}"))
    .fetch_one(&c.e.owner)
    .await
    .unwrap()
}

async fn case(c: &Ctx, tenant: Uuid, job: Uuid, n: i64, title: &str, customer: &str) {
    sqlx::query(
        "INSERT INTO agent_case (tenant_id, job_id, case_id, conversation_id, channel, env, title, customer_name) \
         VALUES ($1, $2, 'CS-' || $3::int8, 'conv-' || $3::int8, 'web_chat', 'prod', $4, $5)",
    )
    .bind(tenant)
    .bind(job)
    .bind(n)
    .bind(title)
    .bind(customer)
    .execute(&c.e.owner)
    .await
    .unwrap();
}

fn hits(b: &Value) -> Vec<(String, String)> {
    b["cases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| {
            (
                h["job"].as_str().unwrap().to_string(),
                h["case_id"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

#[tokio::test]
async fn search_finds_cases_across_jobs_with_job_keys() {
    let c = conv::setup().await;
    let j2 = job(&c, c.t, "j2").await;
    case(&c, c.t, c.job, 1, "Parcel zebra delayed", "Femke").await;
    case(&c, c.t, j2, 1, "Refund zebra", "Jan").await;
    case(&c, c.t, j2, 2, "Something else", "Jan").await;
    let (s, b) = c.get("/api/search?q=ZEBRA", "u-obs").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let mut got = hits(&b);
    got.sort();
    assert_eq!(
        got,
        [("j1".into(), "CS-1".into()), ("j2".into(), "CS-1".into())]
    );
    let h = b["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["job"] == "j2")
        .unwrap();
    assert_eq!(h["href"], "/admin/jobs/j2/cases/CS-1");
    assert_eq!(h["job_name"], "Job j2");
    assert_eq!(h["customer_name"], "Jan");
    assert_eq!(h["status"], "open");
    assert_eq!(h["title"], "Refund zebra");
    // matches the per-job route
    let (_, p) = c.get("/api/jobs/j2/cases?q=zebra", "u-obs").await;
    assert_eq!(p["items"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn search_short_query_is_empty_and_long_is_rejected() {
    let c = conv::setup().await;
    case(&c, c.t, c.job, 1, "Parcel", "Femke").await;
    for q in ["", "p", "%20%20p%20"] {
        let (s, b) = c.get(&format!("/api/search?q={q}"), "u-obs").await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert!(hits(&b).is_empty(), "{b}");
    }
    let (s, b) = c.get("/api/search", "u-obs").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert!(hits(&b).is_empty());
    let (s, b) = c
        .get(&format!("/api/search?q={}", "a".repeat(201)), "u-obs")
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_query")
    );
}

#[tokio::test]
async fn search_never_returns_another_tenants_cases() {
    let c = conv::setup().await;
    case(&c, c.t, c.job, 1, "Parcel zebra", "Femke").await;
    let other = Uuid::new_v4();
    insert_tenant(&c.e.owner, other, "Beta").await;
    let oj = job(&c, other, "j1").await;
    case(&c, other, oj, 7, "Beta zebra secret", "Eve").await;
    let (_, b) = c.get("/api/search?q=zebra", "u-obs").await;
    assert_eq!(hits(&b), [("j1".to_string(), "CS-1".to_string())]);
    let (_, b) = c.get("/api/search?q=secret", "u-obs").await;
    assert!(hits(&b).is_empty(), "{b}");
}

#[tokio::test]
async fn search_is_newest_first_and_capped_at_ten() {
    let c = conv::setup().await;
    for n in 1..=12 {
        case(&c, c.t, c.job, n, "Parcel", "Femke").await;
    }
    sqlx::query(
        "UPDATE agent_case SET updated_at = now() + make_interval(mins => substr(case_id, 4)::int)",
    )
    .execute(&c.e.owner)
    .await
    .unwrap();
    let (_, b) = c.get("/api/search?q=parcel", "u-obs").await;
    let got = hits(&b);
    assert_eq!(got.len(), 10);
    assert_eq!(got[0].1, "CS-12");
    assert_eq!(got[9].1, "CS-3");
}

#[tokio::test]
async fn search_without_auth_is_401() {
    let c = conv::setup().await;
    let (s, _) = send(&c.e.app, get("/api/search?q=parcel", None)).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
}
