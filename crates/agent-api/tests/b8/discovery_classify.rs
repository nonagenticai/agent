//! B8 T14b — discovery classification and clusters (`discovery::classify::LlmClassifier`), against a fake Laya
//! (`POST /v1/systemone`, the platform-tenant job's internal `gates` provider) and a fake OpenAI-compatible server
//! (the cluster drafter). Real Postgres 18; the import and its rows are inserted through the owner pool so no upload
//! worker races the run, and the worker is driven directly (`worker::run`).
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use agent_api::discovery::classify::LlmClassifier;
use agent_api::discovery::worker::{self, ImportRef, Outcome};
use agent_api::generator::llm::OpenAiDrafter;
use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::common::design_contract::{self, ContractJob, DESIGNER};
use crate::common::{self, TestEnv};

const TOKEN: &str = "laya-licence-token-t14b";

// ---- the fake Laya ------------------------------------------------------------------------------------------------

#[derive(Default)]
struct LayaSeen {
    calls: AtomicUsize,
}

/// By the message: `boom` → HTTP 500, `parcel` → the where_is_my_order option, `cancel` → cancel_order, `unsure` → the
/// first option at confidence 0.2, anything else → `none`. Confidence 0.9 unless said.
async fn laya(State(s): State<Arc<LayaSeen>>, Json(b): Json<Value>) -> (StatusCode, Json<Value>) {
    s.calls.fetch_add(1, Ordering::SeqCst);
    let msg = b["state"]["message"]
        .as_str()
        .unwrap_or_default()
        .to_lowercase();
    if msg.contains("boom") {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({})));
    }
    let keys: Vec<String> = b["questions"]["activity"]["criteria"]
        .as_object()
        .expect("criteria")
        .keys()
        .cloned()
        .collect();
    let pick = |needle: &str| keys.iter().find(|k| k.contains(needle)).cloned();
    let (choice, conf) = if msg.contains("unsure") {
        (keys[0].clone(), 0.2)
    } else if msg.contains("parcel") {
        (
            pick("where_is_my_order").expect("where_is_my_order is offered"),
            0.9,
        )
    } else if msg.contains("cancel") {
        (pick("cancel_order").expect("cancel_order is offered"), 0.9)
    } else {
        ("none".to_owned(), 0.9)
    };
    let probs: serde_json::Map<String, Value> = keys
        .iter()
        .map(|k| (k.clone(), json!(if *k == choice { 0.9 } else { 0.01 })))
        .collect();
    (
        StatusCode::OK,
        Json(json!({
            "model": "laya-jev-test",
            "answers": {"activity": {"type": "choice", "choice": choice, "probabilities": probs, "confidence": conf}},
            "usage": {"input_tokens": 42, "output_tokens": 3},
        })),
    )
}

async fn laya_fake() -> (String, Arc<LayaSeen>) {
    let seen = Arc::new(LayaSeen::default());
    let app = Router::new()
        .route("/v1/systemone", post(laya))
        .with_state(seen.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (format!("http://{addr}"), seen)
}

// ---- the fake cluster drafter -------------------------------------------------------------------------------------

#[derive(Default)]
struct ChatSeen {
    calls: AtomicUsize,
    fail: bool,
}

/// One cluster of every message that mentions `lawn`; usage 100 / 20 tokens.
async fn chat(State(s): State<Arc<ChatSeen>>, Json(b): Json<Value>) -> (StatusCode, Json<Value>) {
    s.calls.fetch_add(1, Ordering::SeqCst);
    if s.fail {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({})));
    }
    let input: Value =
        serde_json::from_str(b["messages"][1]["content"].as_str().expect("user message")).unwrap();
    let members: Vec<Value> = input["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .filter(|m| m["text"].as_str().unwrap_or_default().contains("lawn"))
        .map(|m| m["id"].clone())
        .collect();
    let content = json!({"items": [{
        "kind": "discovery_cluster", "title": "Garden and lawn", "detail": "Customers ask about lawn care",
        "payload": {"label": "Lawn care", "members": members,
                    "suggestion": {"kind": "new_activity", "stage": "delivery", "slug": "lawn_care", "label": "Lawn care help"}},
        "example": null, "new_object": null, "confidence": 0.8
    }]})
    .to_string();
    (
        StatusCode::OK,
        Json(json!({
            "choices": [{"message": {"role": "assistant", "content": content}}],
            "usage": {"prompt_tokens": 100, "completion_tokens": 20},
        })),
    )
}

async fn chat_fake(fail: bool) -> (String, Arc<ChatSeen>) {
    let seen = Arc::new(ChatSeen {
        calls: AtomicUsize::new(0),
        fail,
    });
    let app = Router::new()
        .route("/v1/chat/completions", post(chat))
        .with_state(seen.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (format!("http://{addr}/v1"), seen)
}

// ---- fixture ------------------------------------------------------------------------------------------------------

struct Fx {
    e: TestEnv,
    f: ContractJob,
    laya: Arc<LayaSeen>,
}

/// A platform-tenant job: its `gates` use resolves to the internal Laya, the fake.
async fn setup() -> Fx {
    let (base, laya) = laya_fake().await;
    let tenant = Uuid::new_v4();
    let e = common::env_with(|vars| {
        vars.insert("AGENT_PLATFORM_TENANT_ID".into(), tenant.to_string());
        vars.insert("INTERNAL_LAYA_URL".into(), base);
        vars.insert("INTERNAL_LAYA_KEY".into(), TOKEN.into());
    })
    .await;
    let t = crate::b8::support::content(&e).await;
    let f = design_contract::new_job_for_on(&e, tenant, t).await;
    crate::b8::support::route_content_ops(&e, &f).await;
    Fx { e, f, laya }
}

fn drafter(url: &str) -> Arc<OpenAiDrafter> {
    Arc::new(
        OpenAiDrafter::direct(
            url,
            "sk-test",
            "gpt-4.1",
            "prov-1",
            "platform_external",
            Duration::from_secs(10),
        )
        .unwrap(),
    )
}

/// An import of `(text, weight)` rows, status `classifying`, uploaded by [`DESIGNER`]. Returns its id.
async fn seed(f: &ContractJob, e: &TestEnv, rows: &[(&str, i32)]) -> String {
    let total: i32 = rows.iter().map(|r| r.1).sum();
    let id: String = sqlx::query_scalar(
        "INSERT INTO discovery_import (tenant_id, job_id, file, rows, distinct_rows, uploaded_by) \
         VALUES ($1, $2, 'tickets.csv', $3, $4, $5) RETURNING id",
    )
    .bind(f.tenant)
    .bind(f.job.job_id)
    .bind(total)
    .bind(i32::try_from(rows.len()).unwrap())
    .bind(DESIGNER)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    for (i, (text, weight)) in rows.iter().enumerate() {
        sqlx::query(
            "INSERT INTO discovery_row (tenant_id, job_id, import_id, text_hash, ord, excerpt, weight) \
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(f.tenant)
        .bind(f.job.job_id)
        .bind(&id)
        .bind(format!("{:064x}", i + 1))
        .bind(i32::try_from(i).unwrap())
        .bind(*text)
        .bind(*weight)
        .execute(&e.owner)
        .await
        .unwrap();
    }
    id
}

fn at(f: &ContractJob, id: &str) -> ImportRef {
    ImportRef {
        tenant: f.tenant,
        job_id: f.job.job_id,
        id: id.to_owned(),
    }
}

/// `(excerpt, bucket, stage, activity, confidence, cluster_id)` by `ord`.
async fn buckets(
    e: &TestEnv,
    id: &str,
) -> Vec<(
    String,
    String,
    Option<String>,
    Option<String>,
    Option<f32>,
    Option<String>,
)> {
    sqlx::query_as(
        "SELECT excerpt, bucket, stage, activity, confidence, cluster_id FROM discovery_row \
          WHERE import_id = $1 ORDER BY ord",
    )
    .bind(id)
    .fetch_all(&e.owner)
    .await
    .unwrap()
}

async fn status(e: &TestEnv, id: &str) -> String {
    sqlx::query_scalar("SELECT status FROM discovery_import WHERE id = $1")
        .bind(id)
        .fetch_one(&e.owner)
        .await
        .unwrap()
}

// ---- tests --------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn laya_error_fails_open_to_unclear() {
    let x = setup().await;
    let (url, _chat) = chat_fake(false).await;
    let clf = LlmClassifier::new(x.e.state.clone()).with_drafter(drafter(&url));
    let id = seed(
        &x.f,
        &x.e,
        &[
            ("where is my parcel", 1),
            ("boom goes the laya", 4),
            ("cancel my order", 1),
        ],
    )
    .await;
    let out = worker::run(&x.e.state.db, &clf, &at(&x.f, &id)).await;
    assert_eq!(
        out,
        Outcome::Classified,
        "a Laya error never fails the import"
    );
    assert_eq!(status(&x.e, &id).await, "classified");
    let rows = buckets(&x.e, &id).await;
    assert_eq!(rows[0].1, "covered");
    assert_eq!(rows[1].1, "unclear", "{rows:?}");
    assert_eq!((rows[1].2.as_deref(), rows[1].3.as_deref()), (None, None));
    assert_eq!(rows[2].1, "covered");
    let (coverage, long_tail): (Option<f32>, i32) =
        sqlx::query_as("SELECT coverage, long_tail FROM discovery_import WHERE id = $1")
            .bind(&id)
            .fetch_one(&x.e.owner)
            .await
            .unwrap();
    assert_eq!(long_tail, 4, "the failed row's weight is the long tail");
    assert!((coverage.unwrap() - 2.0 / 6.0).abs() < 1e-6, "{coverage:?}");
}

#[tokio::test]
async fn classified_rows_land_in_effective_activities() {
    let x = setup().await;
    let (url, _chat) = chat_fake(false).await;
    let clf = LlmClassifier::new(x.e.state.clone()).with_drafter(drafter(&url));
    let id = seed(
        &x.f,
        &x.e,
        &[
            ("where is my parcel", 3),
            ("please cancel my order", 2),
            ("unsure what this is", 1),
        ],
    )
    .await;
    assert_eq!(
        worker::run(&x.e.state.db, &clf, &at(&x.f, &id)).await,
        Outcome::Classified
    );
    let rows = buckets(&x.e, &id).await;
    assert_eq!(
        (
            rows[0].1.as_str(),
            rows[0].2.as_deref(),
            rows[0].3.as_deref()
        ),
        (
            "covered",
            Some("delivery"),
            Some("delivery:where_is_my_order")
        )
    );
    assert_eq!(
        (
            rows[1].1.as_str(),
            rows[1].2.as_deref(),
            rows[1].3.as_deref()
        ),
        ("covered", Some("change"), Some("change:cancel_order"))
    );
    assert!(rows[0].4.unwrap() >= 0.9);
    assert_eq!(rows[2].1, "unclear", "below the margin");
    assert!(
        (rows[2].4.unwrap() - 0.2).abs() < 1e-6,
        "its confidence is kept"
    );
    let (coverage, long_tail, volumes): (Option<f32>, i32, Value) =
        sqlx::query_as("SELECT coverage, long_tail, volumes FROM discovery_import WHERE id = $1")
            .bind(&id)
            .fetch_one(&x.e.owner)
            .await
            .unwrap();
    assert!((coverage.unwrap() - 5.0 / 6.0).abs() < 1e-6, "{coverage:?}");
    assert_eq!(long_tail, 1);
    let v = volumes.as_array().unwrap();
    assert_eq!(v.len(), 2, "{volumes}");
    assert_eq!(
        (v[0]["activity"].as_str(), v[0]["tickets"].as_i64()),
        (Some("delivery:where_is_my_order"), Some(3))
    );
    assert_eq!(v[0]["enabled"], json!(true));
}

#[tokio::test]
async fn uncovered_rows_are_clustered() {
    let x = setup().await;
    let (url, chat) = chat_fake(false).await;
    let clf = LlmClassifier::new(x.e.state.clone()).with_drafter(drafter(&url));
    let id = seed(
        &x.f,
        &x.e,
        &[
            ("where is my parcel", 1),
            ("my lawn mower is broken", 5),
            ("lawn seed for sale?", 2),
            ("something else entirely", 1),
        ],
    )
    .await;
    assert_eq!(
        worker::run(&x.e.state.db, &clf, &at(&x.f, &id)).await,
        Outcome::Classified
    );
    assert_eq!(chat.calls.load(Ordering::SeqCst), 1, "one drafting call");
    let rows = buckets(&x.e, &id).await;
    assert_eq!(rows[1].1, "uncovered");
    assert_eq!(rows[3].1, "uncovered");
    let clusters: Vec<(String, String, i32, Value, Value, i32)> = sqlx::query_as(
        "SELECT id, label, tickets, examples, suggestion, drafted_tokens FROM discovery_cluster \
          WHERE import_id = $1 ORDER BY id",
    )
    .bind(&id)
    .fetch_all(&x.e.owner)
    .await
    .unwrap();
    assert_eq!(clusters.len(), 1, "{clusters:?}");
    let (cid, label, tickets, examples, suggestion, tokens) = &clusters[0];
    assert_eq!(cid, &format!("{id}-c1"));
    assert_eq!(label, "Lawn care");
    assert_eq!(*tickets, 7, "the members' weight");
    assert_eq!(
        examples,
        &json!(["my lawn mower is broken", "lawn seed for sale?"]),
        "heaviest first"
    );
    assert_eq!(suggestion["kind"], "tenant_extension");
    assert_eq!(suggestion["stage"], "delivery");
    assert_eq!(suggestion["slug"], "lawn_care");
    assert_eq!(*tokens, 120);
    assert_eq!(rows[1].5.as_deref(), Some(cid.as_str()));
    assert_eq!(rows[2].5.as_deref(), Some(cid.as_str()));
    assert_eq!(rows[3].5, None, "a row no group took stays unclustered");
    assert_eq!(rows[0].5, None);
}

#[tokio::test]
async fn cluster_failure_still_finishes() {
    let x = setup().await;
    let (url, chat) = chat_fake(true).await;
    let clf = LlmClassifier::new(x.e.state.clone()).with_drafter(drafter(&url));
    let id = seed(
        &x.f,
        &x.e,
        &[("where is my parcel", 1), ("my lawn mower is broken", 5)],
    )
    .await;
    assert_eq!(
        worker::run(&x.e.state.db, &clf, &at(&x.f, &id)).await,
        Outcome::Classified
    );
    assert_eq!(chat.calls.load(Ordering::SeqCst), 1);
    assert_eq!(status(&x.e, &id).await, "classified");
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM discovery_cluster WHERE import_id = $1")
        .bind(&id)
        .fetch_one(&x.e.owner)
        .await
        .unwrap();
    assert_eq!(n, 0);
    let rows = buckets(&x.e, &id).await;
    assert_eq!(rows[1].1, "uncovered", "classified all the same");
    let usage: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM usage_event WHERE job_id = $1 AND source = 'discovery' AND \"use\" = 'cluster'",
    )
    .bind(x.f.job.job_id)
    .fetch_one(&x.e.owner)
    .await
    .unwrap();
    assert_eq!(usage, 0, "a drafting call that failed carries no usage");
}

#[tokio::test]
async fn classify_is_metered() {
    let x = setup().await;
    let (url, _chat) = chat_fake(false).await;
    let clf = LlmClassifier::new(x.e.state.clone()).with_drafter(drafter(&url));
    let id = seed(
        &x.f,
        &x.e,
        &[
            ("where is my parcel", 1),
            ("please cancel my order", 1),
            ("my lawn mower is broken", 1),
            ("boom", 1),
        ],
    )
    .await;
    assert_eq!(
        worker::run(&x.e.state.db, &clf, &at(&x.f, &id)).await,
        Outcome::Classified
    );
    assert_eq!(
        x.laya.calls.load(Ordering::SeqCst),
        4,
        "one Laya call per row"
    );
    let rows: Vec<(String, String, Option<String>, i64, i64, String, String, Uuid, Option<Uuid>)> =
        sqlx::query_as(
            "SELECT source, \"use\", activity, tokens_in, tokens_out, actor, funding, tenant_id, job_id \
               FROM usage_event WHERE job_id = $1 AND source = 'discovery' ORDER BY id",
        )
        .bind(x.f.job.job_id)
        .fetch_all(&x.e.owner)
        .await
        .unwrap();
    let classify: Vec<_> = rows.iter().filter(|r| r.1 == "classify").collect();
    assert_eq!(
        classify.len(),
        3,
        "the answered calls are metered, the failed one is not: {rows:?}"
    );
    for r in &classify {
        assert_eq!((r.3, r.4), (42, 3));
        assert_eq!(r.5, DESIGNER, "attributed to the uploader");
        assert_eq!(r.6, "local_gpu", "the internal Laya's funding");
        assert_eq!((r.7, r.8), (x.f.tenant, Some(x.f.job.job_id)));
    }
    let mut acts: Vec<Option<&str>> = classify.iter().map(|r| r.2.as_deref()).collect();
    acts.sort();
    assert_eq!(
        acts,
        [
            None,
            Some("change:cancel_order"),
            Some("delivery:where_is_my_order")
        ],
        "the qualified activity rides on the event; an uncovered row has none"
    );
    let cluster: Vec<_> = rows.iter().filter(|r| r.1 == "cluster").collect();
    assert_eq!(cluster.len(), 1, "{rows:?}");
    assert_eq!((cluster[0].3, cluster[0].4), (100, 20));
}

#[tokio::test]
async fn without_a_gates_provider_the_import_parks() {
    // an ordinary tenant: nothing binds `gates`, so the classifier cannot serve (and calls nothing)
    let e = common::env().await;
    let f = design_contract::new_job(&e).await;
    let clf = LlmClassifier::new(e.state.clone());
    let id = seed(&f, &e, &[("where is my parcel", 1)]).await;
    assert_eq!(
        worker::run(&e.state.db, &clf, &at(&f, &id)).await,
        Outcome::Parked
    );
    assert_eq!(status(&e, &id).await, "classifying");
    let pending: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM discovery_row WHERE import_id = $1 AND bucket IS NULL",
    )
    .bind(&id)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(pending, 1, "parked: nothing was written for the row");
}
