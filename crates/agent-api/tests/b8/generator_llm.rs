//! B8 T8: the generator drafter against a fake OpenAI-compatible server, and its metering (one `usage_event` + one
//! `generator_cost_line` per call; tenant runs to the tenant, platform runs to the platform-owned tenant).
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use agent_api::error::ApiError;
use agent_api::generator::llm::{self, Draft, DraftError, OpenAiDrafter, StepContext};
use agent_api::generator::usage::{RunRef, meter};
use agent_api::journey::model::GeneratorStepKey;
use agent_api::usage::MeteringConfig;
use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::routing::post;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::common;

#[derive(Clone, Copy)]
enum Mode {
    Good,
    Malformed,
    Slow,
}

struct Fake {
    mode: Mode,
    calls: AtomicUsize,
}

async fn completions(State(f): State<Arc<Fake>>, Json(body): Json<Value>) -> Json<Value> {
    f.calls.fetch_add(1, Ordering::SeqCst);
    assert!(
        body["messages"][0]["content"]
            .as_str()
            .unwrap_or_default()
            .contains("\"items\"")
    );
    let content = match f.mode {
        Mode::Malformed => "I would suggest some activities.".to_owned(),
        Mode::Slow => {
            tokio::time::sleep(Duration::from_secs(5)).await;
            "{\"items\": []}".to_owned()
        }
        Mode::Good => json!({"items": [{
            "kind": "activity", "title": "Track order", "detail": "Where is my parcel",
            "payload": {"key": "delivery:track", "stage": "delivery"}, "example": null,
            "new_object": null, "confidence": 0.9
        }]})
        .to_string(),
    };
    Json(json!({
        "choices": [{"message": {"role": "assistant", "content": content}}],
        "usage": {"prompt_tokens": 100, "completion_tokens": 20},
    }))
}

async fn fake(mode: Mode) -> (String, Arc<Fake>) {
    let f = Arc::new(Fake {
        mode,
        calls: AtomicUsize::new(0),
    });
    let app = Router::new()
        .route("/v1/chat/completions", post(completions))
        .with_state(f.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (format!("http://{addr}/v1"), f)
}

fn drafter(url: &str, funding: &str) -> OpenAiDrafter {
    OpenAiDrafter::direct(
        url,
        "sk-test",
        "gpt-4.1",
        "prov-1",
        funding,
        Duration::from_secs(10),
    )
    .unwrap()
}

fn ctx() -> StepContext {
    StepContext {
        industry: "retail".into(),
        department: "customer_service".into(),
        job: "retail_agent".into(),
        description: "An online shop".into(),
        ..StepContext::default()
    }
}

const BUDGET: Duration = Duration::from_secs(10);

#[tokio::test]
async fn malformed_output_is_unavailable_and_stores_nothing() {
    let (url, f) = fake(Mode::Malformed).await;
    let r = llm::draft(
        &drafter(&url, "platform_external"),
        GeneratorStepKey::Objects,
        &ctx(),
        BUDGET,
    )
    .await;
    assert!(matches!(r, Err(DraftError::Malformed(_))), "{r:?}");
    assert!(matches!(
        ApiError::from(r.unwrap_err()),
        ApiError::Unavailable("unavailable", _, None)
    ));
    assert_eq!(f.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn timeout_is_unavailable() {
    let (url, _f) = fake(Mode::Slow).await;
    let r = llm::draft(
        &drafter(&url, "platform_external"),
        GeneratorStepKey::Operations,
        &ctx(),
        Duration::from_secs(1),
    )
    .await;
    assert_eq!(r, Err(DraftError::Timeout));
}

#[tokio::test]
async fn non_drafting_steps_call_nothing() {
    let (url, f) = fake(Mode::Good).await;
    for s in [
        GeneratorStepKey::Input,
        GeneratorStepKey::Validate,
        GeneratorStepKey::Package,
    ] {
        let d = llm::draft(&drafter(&url, "local_gpu"), s, &ctx(), BUDGET)
            .await
            .unwrap();
        assert_eq!(d, Draft::default());
    }
    assert_eq!(f.calls.load(Ordering::SeqCst), 0);
}

/// A tenant with one job; returns (tenant, job).
async fn tenant_and_job(e: &common::TestEnv, name: &str) -> (Uuid, Uuid) {
    let tenant = Uuid::now_v7();
    common::insert_tenant(&e.owner, tenant, name).await;
    let job: Uuid = sqlx::query_scalar(
        "INSERT INTO jobs (tenant_id, key, label, pins, created_by) VALUES ($1, 'retail_agent', 'Retail', '[]', 'test') \
         RETURNING id",
    )
    .bind(tenant)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    (tenant, job)
}

async fn insert_run(e: &common::TestEnv, tenant: Uuid, job: Option<Uuid>) -> i64 {
    let scope = if job.is_some() { "tenant" } else { "platform" };
    sqlx::query_scalar(
        "INSERT INTO generator_run (tenant_id, scope, industry, department, job_key, job_id, current_step, funding, \
                                    started_by) \
         VALUES ($1, $2, 'retail', 'customer_service', 'retail_agent', $3, 'journey', 'platform_external', 'sa-1') \
         RETURNING id",
    )
    .bind(tenant)
    .bind(scope)
    .bind(job)
    .fetch_one(&e.owner)
    .await
    .unwrap()
}

/// Draft the journey step (3 samples + 1 merge) and meter it on a connection scoped to `tenant` (and `job`).
async fn draft_and_meter(e: &common::TestEnv, tenant: Uuid, job: Option<Uuid>, run: i64) -> i64 {
    let (url, f) = fake(Mode::Good).await;
    let d = llm::draft(
        &drafter(&url, "platform_external"),
        GeneratorStepKey::Journey,
        &ctx(),
        BUDGET,
    )
    .await
    .unwrap();
    assert_eq!(f.calls.load(Ordering::SeqCst), 4);
    assert_eq!(d.calls.len(), 4);
    assert_eq!(d.items.len(), 1);
    let mut tx = agent_api::tenancy::begin_scoped(&e.state.db, tenant)
        .await
        .unwrap();
    if let Some(j) = job {
        sqlx::query("SELECT set_config('app.current_job', $1, true)")
            .bind(j.to_string())
            .execute(&mut *tx)
            .await
            .unwrap();
    }
    let tokens = meter(
        &mut tx,
        &MeteringConfig::default(),
        RunRef {
            id: run,
            job_id: job,
        },
        GeneratorStepKey::Journey,
        "sa-1",
        &d,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    tokens
}

#[tokio::test]
async fn each_call_emits_one_usage_event() {
    let e = common::env().await;
    let (tenant, _) = tenant_and_job(&e, "Platform").await;
    let run = insert_run(&e, tenant, None).await;
    assert_eq!(draft_and_meter(&e, tenant, None, run).await, 4 * 120);
    let uses: Vec<(String, String, String, i64)> = sqlx::query_as(
        "SELECT source, use, funding, tokens_in FROM usage_event WHERE tenant_id = $1 ORDER BY use",
    )
    .bind(tenant)
    .fetch_all(&e.owner)
    .await
    .unwrap();
    assert_eq!(uses.len(), 4);
    assert!(
        uses.iter()
            .all(|u| u.0 == "generator" && u.2 == "platform_external" && u.3 == 100)
    );
    assert_eq!(uses.iter().filter(|u| u.1 == "journey").count(), 3);
    assert_eq!(uses.iter().filter(|u| u.1 == "journey_merge").count(), 1);
    // one cost line per call, each mirroring its usage row
    let lines: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM generator_cost_line l JOIN usage_event u ON u.event_key = l.usage_event_id \
          WHERE l.run_id = $1 AND l.step = 'journey' AND l.tokens = 120",
    )
    .bind(run)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(lines, 4);
}

#[tokio::test]
async fn tenant_run_usage_is_attributed_to_tenant() {
    let e = common::env().await;
    let (tenant, job) = tenant_and_job(&e, "Demo Retail").await;
    let run = insert_run(&e, tenant, Some(job)).await;
    draft_and_meter(&e, tenant, Some(job), run).await;
    let rows: Vec<(Uuid, Option<Uuid>)> =
        sqlx::query_as("SELECT tenant_id, job_id FROM usage_event WHERE source = 'generator'")
            .fetch_all(&e.owner)
            .await
            .unwrap();
    assert_eq!(rows.len(), 4);
    assert!(rows.iter().all(|r| *r == (tenant, Some(job))));
}

#[tokio::test]
async fn platform_run_usage_goes_to_platform_tenant() {
    let e = common::env().await;
    let (platform, _) = tenant_and_job(&e, "Platform").await;
    let (other, _) = tenant_and_job(&e, "Demo Retail").await;
    let run = insert_run(&e, platform, None).await;
    draft_and_meter(&e, platform, None, run).await;
    let rows: Vec<(Uuid, Option<Uuid>)> =
        sqlx::query_as("SELECT tenant_id, job_id FROM usage_event WHERE source = 'generator'")
            .fetch_all(&e.owner)
            .await
            .unwrap();
    assert_eq!(rows.len(), 4);
    assert!(rows.iter().all(|r| *r == (platform, None)));
    assert!(rows.iter().all(|r| r.0 != other));
}
