//! B8 T9b / T10 / T10a / T10b / T11 — generator start, drive, proposals, evidence, package and the tenant-driven
//! routes. Real Postgres 18 through the HTTP router; model calls go to a fake OpenAI-compatible server (the drafter is
//! injected into `drive::draft`) and the evidence classifier to a fake Laya. Runs are started through the API
//! (platform) or written with `store::insert_run` through the owner pool (tenant: starting one needs a bound
//! `generator` provider, which `tenant_run_without_generator_use_409_generator_provider_missing` covers).
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use agent_api::audit::Actor;
use agent_api::generator::llm::{DraftError, DraftItem, Drafter, OpenAiDrafter};
use agent_api::generator::prompts::Call;
use agent_api::generator::run::{self, Scope};
use agent_api::generator::store::{self, NewRun};
use agent_api::generator::{drive, evidence};
use agent_api::journey::grid;
use agent_api::platform_db::begin_platform;
use agent_api::usage::LlmUsage;
use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::common::design_contract::{self, ContractJob};
use crate::common::{self, TestEnv, get, insert_tenant, insert_user, person, send, super_admin};

const DEPT_LAYER: &str = "industry_dept/retail/customer_service";

struct Fx {
    e: TestEnv,
    platform: Uuid,
    f: ContractJob,
}

fn code(b: &Value) -> &str {
    b["error"]["code"].as_str().unwrap_or("")
}

/// A job on the B8 journey content (`support::content`: stages, activities, the `order` object, `order.read`),
/// created BEFORE the empty stage `purchase` is added to the base layer: the platform runs below see `purchase` as an
/// in-scope stage with no activity (rule 4 red until the run marks it out of scope), the job's own journey is
/// unchanged.
async fn journey_job(e: &TestEnv) -> ContractJob {
    let f = crate::b8::support::new_job(e).await;
    crate::b8::support::add(
        e,
        "base",
        "journey_stage",
        "purchase",
        json!({"label": "Purchase", "order": 9, "description": "Buying"}),
    )
    .await;
    crate::b8::support::tag(e, "base").await;
    f
}

async fn setup() -> Fx {
    let platform = Uuid::new_v4();
    let e = common::env_with(|v| {
        v.insert("AGENT_PLATFORM_TENANT_ID".into(), platform.to_string());
    })
    .await;
    insert_tenant(&e.owner, platform, "Platform").await;
    let f = journey_job(&e).await;
    insert_user(&e.owner, "u-jd", f.tenant, "job_designer").await;
    insert_user(&e.owner, "u-op", f.tenant, "operator").await;
    Fx { e, platform, f }
}

fn jd() -> String {
    person("u-jd", &[])
}

async fn call(x: &Fx, method: &str, uri: &str, tok: &str, body: Value) -> (StatusCode, Value) {
    send(&x.e.app, common::json(method, uri, Some(tok), body)).await
}

async fn sa(x: &Fx, method: &str, uri: &str, body: Value) -> (StatusCode, Value) {
    call(x, method, uri, &super_admin(), body).await
}

/// A platform run on retail / customer_service, started through the API.
async fn start(x: &Fx) -> Value {
    let (s, b) = sa(
        x,
        "POST",
        "/api/generator/runs",
        json!({"scope":"platform","industry":"retail","department":"customer_service","job":"gen_probe","description":"a shop"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    b
}

fn run_id(b: &Value) -> i64 {
    b["id"].as_i64().expect("run id")
}

/// Move the run's current step (owner pool): earlier steps done, the step current, later todo.
async fn goto(x: &Fx, id: i64, step: &str) {
    sqlx::query("UPDATE generator_run SET current_step = $2 WHERE id = $1")
        .bind(id)
        .bind(step)
        .execute(&x.e.owner)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE generator_step SET status = CASE WHEN ord < (SELECT ord FROM generator_step WHERE run_id = $1 AND step = $2) \
                                                 THEN 'done' WHEN step = $2 THEN 'current' ELSE 'todo' END WHERE run_id = $1",
    )
    .bind(id)
    .bind(step)
    .execute(&x.e.owner)
    .await
    .unwrap();
}

/// An item of `step` (owner pool). Returns its id.
#[allow(clippy::too_many_arguments)]
async fn item(
    x: &Fx,
    run: i64,
    step: &str,
    kind: &str,
    title: &str,
    decision: &str,
    payload: Value,
    new_object: Option<&str>,
) -> String {
    let n: i64 = sqlx::query_scalar("SELECT count(*) + 1 FROM generator_item WHERE run_id = $1")
        .bind(run)
        .fetch_one(&x.e.owner)
        .await
        .unwrap();
    let id = format!("{run}-{step}-{n}");
    sqlx::query(
        "INSERT INTO generator_item (run_id, item_id, ord, step, kind, title, decision, payload, new_object, example) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
    )
    .bind(run)
    .bind(&id)
    .bind(i32::try_from(n).unwrap())
    .bind(step)
    .bind(kind)
    .bind(title)
    .bind(decision)
    .bind(payload)
    .bind(new_object)
    .bind(new_object.map(|_| json!({"id": "r-1", "total": 4.5, "paid": true})))
    .execute(&x.e.owner)
    .await
    .unwrap();
    id
}

async fn stage_keys(x: &Fx, run: i64) -> Vec<String> {
    sqlx::query_scalar("SELECT stage FROM generator_stage WHERE run_id = $1 ORDER BY ord")
        .bind(run)
        .fetch_all(&x.e.owner)
        .await
        .unwrap()
}

async fn all_out_of_scope(x: &Fx, run: i64) {
    for s in stage_keys(x, run).await {
        let (st, b) = sa(
            x,
            "PUT",
            &format!("/api/platform/generator/runs/{run}/stages/{s}"),
            json!({"out_of_scope": true}),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
    }
}

async fn revisions(x: &Fx, author: &str) -> Vec<(String, String, String)> {
    sqlx::query_as(
        "SELECT r.kind, r.key, r.op FROM layer_row r WHERE r.author = $1 ORDER BY r.created_at, r.key",
    )
    .bind(author)
    .fetch_all(&x.e.owner)
    .await
    .unwrap()
}

async fn tags(x: &Fx, family: &str) -> Vec<(i32, bool)> {
    sqlx::query_as("SELECT version, provisional FROM layer_tag WHERE family = $1 ORDER BY version")
        .bind(family)
        .fetch_all(&x.e.owner)
        .await
        .unwrap()
}

// ---- tenant runs written through the owner pool ------------------------------------------------------------------------

async fn tenant_run(x: &Fx, tenant: Uuid, job: Uuid, job_key: &str) -> i64 {
    let steps: Vec<&'static str> = run::steps_for(Scope::Tenant)
        .iter()
        .map(|s| s.as_str())
        .collect();
    let stages = vec![("purchase".to_owned(), "Purchase".to_owned(), false)];
    let provider = json!({"provider_id": "p-1", "model": "m", "funding": "tenant_byo"});
    let mut c = x.e.owner.acquire().await.unwrap();
    store::insert_run(
        &mut c,
        &NewRun {
            tenant_id: tenant,
            scope: "tenant",
            industry: "retail",
            department: "customer_service",
            job_key,
            job_id: Some(job),
            first_step: steps[1],
            description: "refine",
            output: &["refinements for the job (D8)".to_owned()],
            funding: "tenant_byo",
            provider: Some(&provider),
            started_by: "u-jd",
        },
        &steps,
        "retail / customer_service",
        &stages,
    )
    .await
    .unwrap()
}

fn tpath(x: &Fx, id: i64, tail: &str) -> String {
    format!("/api/jobs/{}/generator/runs/{id}{tail}", x.f.job.job_key)
}

// ---- a fake drafter ----------------------------------------------------------------------------------------------------

struct Fixed(Vec<DraftItem>);

impl Drafter for Fixed {
    fn call<'a>(
        &'a self,
        _c: Call,
        _i: &'a Value,
        _s: usize,
    ) -> BoxFuture<'a, Result<(Vec<DraftItem>, LlmUsage), DraftError>> {
        let items = self.0.clone();
        Box::pin(async move {
            Ok((
                items,
                LlmUsage {
                    provider_id: "prov-1".into(),
                    model: "m".into(),
                    funding: "platform_external".into(),
                    tokens_in: 10,
                    tokens_out: 2,
                    latency_ms: 1,
                },
            ))
        })
    }
}

fn draft_item(kind: &str, title: &str) -> DraftItem {
    DraftItem {
        kind: kind.into(),
        title: title.into(),
        detail: String::new(),
        example: None,
        payload: json!({}),
        new_object: None,
        confidence: 0.9,
    }
}

/// Drive `drive::draft` for a platform run the way the route does.
async fn platform_draft(
    x: &Fx,
    id: i64,
    hint: Option<&str>,
    d: &dyn Drafter,
) -> agent_api::error::ApiResult<Value> {
    let mut tx = begin_platform(&x.e.state.db).await.unwrap();
    let run = drive::platform_run(&mut tx, id).await.unwrap();
    agent_api::tenancy::set_tenant(&mut tx, run.tenant_id)
        .await
        .unwrap();
    let r = drive::draft(
        &mut tx,
        &x.e.state,
        &run,
        &Actor::system("service:test"),
        "u-super",
        hint.map(str::to_owned),
        d,
    )
    .await;
    if r.is_ok() {
        tx.commit().await.unwrap();
    }
    r
}

// ---- T9b start ---------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn generator_start_creates_one_stage_row_per_effective_stage() {
    let x = setup().await;
    let b = start(&x).await;
    let id = run_id(&b);
    let mut tx = begin_platform(&x.e.state.db).await.unwrap();
    let j = grid::load(&mut tx, "retail", "customer_service")
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    let keys = stage_keys(&x, id).await;
    assert_eq!(
        keys,
        j.stages
            .iter()
            .map(|s| s.key.as_str().to_owned())
            .collect::<Vec<_>>()
    );
    assert_eq!(b["stages"].as_array().unwrap().len(), j.stages.len());
    assert_eq!(
        b["status"]
            .as_str()
            .map(|s| s == "blocked" || s == "running"),
        Some(true),
        "{b}"
    );
    let steps: Vec<(String, String)> =
        sqlx::query_as("SELECT step, status FROM generator_step WHERE run_id = $1 ORDER BY ord")
            .bind(id)
            .fetch_all(&x.e.owner)
            .await
            .unwrap();
    assert_eq!(steps.len(), 9);
    assert_eq!(steps[0], ("input".into(), "done".into()));
    assert_eq!(steps[1].1, "current");
    assert!(
        b["output"][0]
            .as_str()
            .unwrap()
            .starts_with(&format!("{DEPT_LAYER}@v")),
        "{b}"
    );
}

#[tokio::test]
async fn platform_start_needs_a_hierarchy_and_a_super_admin() {
    let x = setup().await;
    let (s, b) = sa(
        &x,
        "POST",
        "/api/generator/runs",
        json!({"scope":"platform","industry":"retail","job":"gen_job","description":""}),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "hierarchy_incomplete"),
        "{b}"
    );
    let (s, b) = call(
        &x,
        "POST",
        "/api/generator/runs",
        &jd(),
        json!({"scope":"platform","industry":"retail","department":"customer_service","job":"gen_job"}),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::FORBIDDEN, "super_admin_required"),
        "{b}"
    );
    let (s, b) = sa(
        &x,
        "POST",
        "/api/generator/runs",
        json!({"scope":"platform","industry":"Retail!","department":"x","job":"gen_job"}),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
}

#[tokio::test]
async fn tenant_run_without_generator_use_409_generator_provider_missing() {
    let x = setup().await;
    let (s, b) = call(
        &x,
        "POST",
        "/api/generator/runs",
        &jd(),
        json!({"scope":"tenant","job":x.f.job.job_key,"description":"x"}),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "generator_provider_missing"),
        "{b}"
    );
    let (s, b) = call(
        &x,
        "POST",
        "/api/generator/runs",
        &person("u-op", &[]),
        json!({"scope":"tenant","job":x.f.job.job_key}),
    )
    .await;
    assert_eq!((s, code(&b)), (StatusCode::FORBIDDEN, "forbidden"), "{b}");
}

// ---- T10 drive ---------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn undecided_items_409() {
    let x = setup().await;
    let id = run_id(&start(&x).await);
    goto(&x, id, "journey").await;
    let it = item(
        &x,
        id,
        "journey",
        "activity",
        "purchase:probe",
        "pending",
        json!({}),
        None,
    )
    .await;
    let (s, b) = sa(
        &x,
        "POST",
        &format!("/api/platform/generator/runs/{id}/advance"),
        json!(null),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "undecided_items"),
        "{b}"
    );
    let (s, b) = sa(
        &x,
        "POST",
        &format!("/api/platform/generator/runs/{id}/items/{it}/decision"),
        json!({"decision":"dropped"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

#[tokio::test]
async fn rename_activity_key_validated_and_merge_needs_target() {
    let x = setup().await;
    let id = run_id(&start(&x).await);
    goto(&x, id, "journey").await;
    let a = item(
        &x,
        id,
        "journey",
        "activity",
        "purchase:a",
        "pending",
        json!({}),
        None,
    )
    .await;
    let b2 = item(
        &x,
        id,
        "journey",
        "activity",
        "purchase:b",
        "pending",
        json!({}),
        None,
    )
    .await;
    let uri = |i: &str| format!("/api/platform/generator/runs/{id}/items/{i}/decision");
    let (s, b) = sa(&x, "POST", &uri(&a), json!({"decision":"renamed"})).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "note_required"),
        "{b}"
    );
    let (s, b) = sa(
        &x,
        "POST",
        &uri(&a),
        json!({"decision":"renamed","note":"Not A Key"}),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_key"),
        "{b}"
    );
    let (s, b) = sa(
        &x,
        "POST",
        &uri(&a),
        json!({"decision":"renamed","note":"purchase:renamed_one"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = sa(&x, "POST", &uri(&b2), json!({"decision":"merged"})).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "merge_target_required"),
        "{b}"
    );
    let (s, b) = sa(
        &x,
        "POST",
        &uri(&b2),
        json!({"decision":"merged","note":&b2}),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_merge_target"),
        "{b}"
    );
    let (s, b) = sa(
        &x,
        "POST",
        &uri(&b2),
        json!({"decision":"merged","note":"purchase:renamed_one"}),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "item_not_found"),
        "{b}"
    );
    let (s, b) = sa(
        &x,
        "POST",
        &uri(&b2),
        json!({"decision":"merged","note":&a}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

#[tokio::test]
async fn decide_on_a_step_not_reached_is_409() {
    let x = setup().await;
    let id = run_id(&start(&x).await);
    let it = item(
        &x,
        id,
        "use_cases",
        "use_case_card",
        "purchase:x",
        "pending",
        json!({}),
        None,
    )
    .await;
    let (s, b) = sa(
        &x,
        "POST",
        &format!("/api/platform/generator/runs/{id}/items/{it}/decision"),
        json!({"decision":"accepted"}),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "step_not_reached"),
        "{b}"
    );
}

#[tokio::test]
async fn advance_blocked_while_red() {
    let x = setup().await;
    let id = run_id(&start(&x).await);
    goto(&x, id, "validate").await;
    // no stage has an activity: rule 4 is red
    let (s, b) = sa(
        &x,
        "POST",
        &format!("/api/platform/generator/runs/{id}/advance"),
        json!(null),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "validation_failed"),
        "{b}"
    );
    assert!(
        b["error"]["findings"]
            .as_array()
            .is_some_and(|f| !f.is_empty()),
        "{b}"
    );
    all_out_of_scope(&x, id).await;
    let (s, b) = sa(
        &x,
        "POST",
        &format!("/api/platform/generator/runs/{id}/advance"),
        json!(null),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["current_step"], "use_cases", "{b}");
}

#[tokio::test]
async fn advance_writes_untagged_revisions_only() {
    let x = setup().await;
    let before = tags(&x, DEPT_LAYER).await;
    let id = run_id(&start(&x).await);
    let stages = stage_keys(&x, id).await;
    goto(&x, id, "journey").await;
    let key = format!("{}:gen_probe", stages[0]);
    item(
        &x,
        id,
        "journey",
        "activity",
        &key,
        "accepted",
        json!({"label":"Generated probe","actor":"customer","trigger":"asks"}),
        None,
    )
    .await;
    let (s, b) = sa(
        &x,
        "POST",
        &format!("/api/platform/generator/runs/{id}/advance"),
        json!(null),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["current_step"], "objects", "{b}");
    let written = revisions(&x, &format!("generator:{id}")).await;
    assert!(
        written
            .iter()
            .any(|(k, key2, _)| k == "activity" && *key2 == key),
        "{written:?}"
    );
    let after = tags(&x, DEPT_LAYER).await;
    let real = |t: &[(i32, bool)]| t.iter().filter(|(_, p)| !p).cloned().collect::<Vec<_>>();
    assert_eq!(
        real(&after),
        real(&before),
        "no real tag is cut by an advance"
    );
    assert!(after.contains(&(0, true)), "the provisional v0: {after:?}");
    let untagged: i64 = sqlx::query_scalar("SELECT count(*) FROM layer_untagged WHERE family = $1")
        .bind(DEPT_LAYER)
        .fetch_one(&x.e.owner)
        .await
        .unwrap();
    assert!(untagged >= 1);
}

#[tokio::test]
async fn advance_writes_the_payload_key_not_the_human_title() {
    let x = setup().await;
    let id = run_id(&start(&x).await);
    let stages = stage_keys(&x, id).await;
    goto(&x, id, "journey").await;
    let key = format!("{}:keyed_probe", stages[0]);
    item(
        &x,
        id,
        "journey",
        "activity",
        "Keyed probe, a human title",
        "accepted",
        json!({"key": key, "label":"Keyed probe","actor":"customer","trigger":"asks"}),
        None,
    )
    .await;
    let (s, b) = sa(
        &x,
        "POST",
        &format!("/api/platform/generator/runs/{id}/advance"),
        json!(null),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let written = revisions(&x, &format!("generator:{id}")).await;
    assert!(
        written
            .iter()
            .any(|(k, key2, _)| k == "activity" && *key2 == key),
        "{written:?}"
    );
    assert!(
        !written.iter().any(|(_, key2, _)| key2.contains("human")),
        "{written:?}"
    );
}

#[tokio::test]
async fn package_tags_family_provisional() {
    let x = setup().await;
    let id = run_id(&start(&x).await);
    let before = tags(&x, DEPT_LAYER).await;
    goto(&x, id, "journey").await;
    all_out_of_scope(&x, id).await;
    let (s, b) = sa(
        &x,
        "POST",
        &format!("/api/platform/generator/runs/{id}/advance"),
        json!(null),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert!(
        tags(&x, DEPT_LAYER).await.contains(&(0, true)),
        "advance cuts the provisional v0"
    );
    // not at package yet
    let (s, b) = sa(
        &x,
        "POST",
        &format!("/api/platform/generator/runs/{id}/tag"),
        json!(null),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "not_at_package"),
        "{b}"
    );
    goto(&x, id, "package").await;
    let (s, b) = sa(
        &x,
        "POST",
        &format!("/api/platform/generator/runs/{id}/tag"),
        json!(null),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["status"], "packaged", "{b}");
    let after = tags(&x, DEPT_LAYER).await;
    assert!(
        !after.contains(&(0, true)),
        "v0 became a real tag: {after:?}"
    );
    let next = before.iter().map(|t| t.0).max().unwrap_or(0) + 1;
    assert!(after.contains(&(next, false)), "{after:?}");
    assert_eq!(b["tagged"], format!("{DEPT_LAYER}@v{next}"), "{b}");
    let n: i64 =
        sqlx::query_scalar("SELECT count(*) FROM platform_audit WHERE action = 'generator.tag'")
            .fetch_one(&x.e.owner)
            .await
            .unwrap();
    assert_eq!(n, 1, "tag_is_platform_audited");
    // twice
    let (s, b) = sa(
        &x,
        "POST",
        &format!("/api/platform/generator/runs/{id}/tag"),
        json!(null),
    )
    .await;
    assert_eq!((s, code(&b)), (StatusCode::CONFLICT, "run_packaged"), "{b}");
    let (s, b) = sa(
        &x,
        "POST",
        &format!("/api/platform/generator/runs/{id}/advance"),
        json!(null),
    )
    .await;
    assert_eq!((s, code(&b)), (StatusCode::CONFLICT, "run_packaged"), "{b}");
}

#[tokio::test]
async fn package_red_is_409_and_writes_no_tag() {
    let x = setup().await;
    let id = run_id(&start(&x).await);
    let before = tags(&x, DEPT_LAYER).await;
    goto(&x, id, "package").await;
    let (s, b) = sa(
        &x,
        "POST",
        &format!("/api/platform/generator/runs/{id}/tag"),
        json!(null),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "validation_failed"),
        "{b}"
    );
    assert_eq!(tags(&x, DEPT_LAYER).await, before);
    let status: String = sqlx::query_scalar("SELECT status FROM generator_run WHERE id = $1")
        .bind(id)
        .fetch_one(&x.e.owner)
        .await
        .unwrap();
    assert_eq!(status, "running");
}

#[tokio::test]
async fn b6_tag_refused_while_run_red() {
    let x = setup().await;
    let id = run_id(&start(&x).await);
    goto(&x, id, "validate").await;
    // an untagged revision on the department layer, as a hand edit would leave
    let mut tx = begin_platform(&x.e.state.db).await.unwrap();
    agent_api::platform::db::rows::write_revision(
        &mut tx,
        &Actor::system("service:test"),
        DEPT_LAYER,
        agent_model::kinds::Kind::StageScope,
        "purchase",
        agent_model::kinds::Op::Add,
        json!({"out_of_scope": true}),
        "u-hand",
    )
    .await
    .unwrap();
    let refused = agent_api::platform::db::tags::tag_family(
        &mut tx,
        &Actor::system("service:test"),
        DEPT_LAYER,
        "n",
        "u-hand",
        &*x.e.state.generator_guard,
        false,
    )
    .await;
    tx.rollback().await.unwrap();
    let e = refused.expect_err("a red open run holds the family");
    assert_eq!(e.status_and_code().1, "validation_red", "{e:?}");
}

#[tokio::test]
async fn tag_on_a_tenant_run_and_unknown_runs() {
    let x = setup().await;
    let t = tenant_run(&x, x.f.tenant, x.f.job.job_id, &x.f.job.job_key).await;
    for (m, tail, body) in [
        ("POST", "draft", json!({})),
        ("POST", "advance", json!(null)),
        ("POST", "tag", json!(null)),
        ("PUT", "stages/purchase", json!({"out_of_scope": true})),
    ] {
        let (s, b) = sa(
            &x,
            m,
            &format!("/api/platform/generator/runs/{t}/{tail}"),
            body,
        )
        .await;
        assert_eq!(
            (s, code(&b)),
            (StatusCode::CONFLICT, "not_a_platform_run"),
            "{tail}: {b}"
        );
    }
    let (s, b) = sa(
        &x,
        "POST",
        "/api/platform/generator/runs/99999/advance",
        json!(null),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "run_not_found"),
        "{b}"
    );
    // the SA reads still list the tenant run read-only
    let (s, b) = send(
        &x.e.app,
        get(
            &format!("/api/platform/generator/runs/{t}"),
            Some(&super_admin()),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

// ---- T10 proposals -----------------------------------------------------------------------------------------------------

#[tokio::test]
async fn approve_proposal_adds_object_and_clears_rule1() {
    let x = setup().await;
    let id = run_id(&start(&x).await);
    goto(&x, id, "objects").await;
    let it = item(
        &x,
        id,
        "objects",
        "activity_object",
        "purchase:probe × gift_card",
        "accepted",
        json!({}),
        Some("gift_card"),
    )
    .await;
    let d = send(
        &x.e.app,
        get(
            &format!("/api/platform/generator/runs/{id}"),
            Some(&super_admin()),
        ),
    )
    .await
    .1;
    assert!(
        d["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["rule"] == 1),
        "{d}"
    );
    let uri = format!("/api/platform/generator/runs/{id}/items/{it}/proposal");
    let (s, b) = sa(&x, "POST", &uri, json!({})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = sa(&x, "POST", &uri, json!({})).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "already_proposed"),
        "{b}"
    );
    let plain = item(
        &x,
        id,
        "objects",
        "activity_object",
        "purchase:probe × order",
        "accepted",
        json!({}),
        None,
    )
    .await;
    let (s, b) = sa(
        &x,
        "POST",
        &format!("/api/platform/generator/runs/{id}/items/{plain}/proposal"),
        json!({}),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "not_a_new_object"),
        "{b}"
    );
    let (s, b) = sa(
        &x,
        "POST",
        &format!("/api/platform/generator/runs/{id}/items/nope/proposal"),
        json!({}),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "item_not_found"),
        "{b}"
    );
    let p = format!("/api/platform/object-proposals/{id}:{it}");
    let (s, b) = sa(
        &x,
        "POST",
        "/api/platform/object-proposals/nocolon",
        json!({"approve": true}),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "proposal_not_found"),
        "{b}"
    );
    let (s, b) = sa(&x, "POST", &p, json!({"approve": true})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["objects"], json!(["gift_card"]), "{b}");
    assert!(
        !b["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["rule"] == 1),
        "rule 1 clears: {b}"
    );
    let rev = revisions(&x, &format!("generator:{id}")).await;
    assert_eq!(
        rev,
        vec![(
            "object".to_owned(),
            "gift_card".to_owned(),
            "add".to_owned()
        )]
    );
    let (s, b) = sa(&x, "POST", &p, json!({"approve": true})).await;
    assert_eq!((s, code(&b)), (StatusCode::CONFLICT, "not_proposed"), "{b}");
}

#[tokio::test]
async fn reject_proposal_writes_nothing() {
    let x = setup().await;
    let id = run_id(&start(&x).await);
    goto(&x, id, "objects").await;
    let it = item(
        &x,
        id,
        "objects",
        "activity_object",
        "purchase:probe × voucher",
        "accepted",
        json!({}),
        Some("voucher"),
    )
    .await;
    sa(
        &x,
        "POST",
        &format!("/api/platform/generator/runs/{id}/items/{it}/proposal"),
        json!({}),
    )
    .await;
    let (s, b) = sa(
        &x,
        "POST",
        &format!("/api/platform/object-proposals/{id}:{it}"),
        json!({"approve": false}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let p: String = sqlx::query_scalar(
        "SELECT proposal FROM generator_item WHERE run_id = $1 AND item_id = $2",
    )
    .bind(id)
    .bind(&it)
    .fetch_one(&x.e.owner)
    .await
    .unwrap();
    assert_eq!(p, "rejected");
    assert!(revisions(&x, &format!("generator:{id}")).await.is_empty());
}

fn op_body(verb: &str) -> Value {
    json!({"operation": {
        "object": "order", "verb": verb, "identity_level": "1", "side_effect": "R", "confirm_back": false,
        "idempotency": "nat", "description": "A test operation."
    }})
}

#[tokio::test]
async fn operation_proposal_uses_f22_validator() {
    let x = setup().await;
    let id = run_id(&start(&x).await);
    goto(&x, id, "operations").await;
    let it = item(
        &x,
        id,
        "operations",
        "operation_expectation",
        "purchase:probe × order.audit_trail",
        "accepted",
        json!({"op":"order.audit_trail"}),
        None,
    )
    .await;
    let uri = format!("/api/platform/generator/runs/{id}/items/{it}/operation-proposal");
    // not an operations item
    let obj = item(
        &x,
        id,
        "operations",
        "activity_object",
        "x",
        "accepted",
        json!({}),
        None,
    )
    .await;
    let (s, b) = sa(
        &x,
        "POST",
        &format!("/api/platform/generator/runs/{id}/items/{obj}/operation-proposal"),
        op_body("audit_trail"),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "not_an_operations_item"),
        "{b}"
    );
    // invalid: unknown object, bad identity level
    let mut bad = op_body("audit_trail");
    bad["operation"]["object"] = json!("no_such_object");
    let (s, b) = sa(&x, "POST", &uri, bad).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "operation_invalid"),
        "{b}"
    );
    let mut bad = op_body("audit_trail");
    bad["operation"]["identity_level"] = json!("L9");
    let (s, b) = sa(&x, "POST", &uri, bad).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "operation_invalid"),
        "invalid_operation_is_422_with_details {b}"
    );
    // an existing operation
    let (s, b) = sa(&x, "POST", &uri, op_body("read")).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "operation_exists"),
        "{b}"
    );
    // valid
    let (s, b) = sa(&x, "POST", &uri, op_body("audit_trail")).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b2) = sa(&x, "POST", &uri, op_body("audit_trail")).await;
    assert_eq!(
        (s, code(&b2)),
        (StatusCode::CONFLICT, "already_proposed"),
        "{b2}"
    );
    assert!(
        revisions(&x, &format!("generator:{id}")).await.is_empty(),
        "a proposal writes nothing"
    );
    // approve: an untagged revision only
    let tags_before = tags(&x, DEPT_LAYER).await;
    let (s, b) = sa(
        &x,
        "POST",
        &format!("/api/platform/object-proposals/{id}:{it}"),
        json!({"approve": true}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        revisions(&x, &format!("generator:{id}")).await,
        vec![(
            "operation".to_owned(),
            "order.audit_trail".to_owned(),
            "add".to_owned()
        )]
    );
    assert_eq!(tags(&x, DEPT_LAYER).await, tags_before);
    assert!(
        b["output"]
            .as_array()
            .unwrap()
            .iter()
            .any(|o| o == "operation:order.audit_trail"),
        "{b}"
    );
}

#[tokio::test]
async fn reject_operation_writes_nothing() {
    let x = setup().await;
    let id = run_id(&start(&x).await);
    goto(&x, id, "operations").await;
    let it = item(
        &x,
        id,
        "operations",
        "operation_expectation",
        "purchase:probe × order.audit_trail",
        "accepted",
        json!({}),
        None,
    )
    .await;
    let (s, b) = sa(
        &x,
        "POST",
        &format!("/api/platform/generator/runs/{id}/items/{it}/operation-proposal"),
        op_body("audit_trail"),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = sa(
        &x,
        "POST",
        &format!("/api/platform/object-proposals/{id}:{it}"),
        json!({"approve": false}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert!(revisions(&x, &format!("generator:{id}")).await.is_empty());
}

// ---- T10a hint ---------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn draft_hint_too_long_422() {
    let x = setup().await;
    let id = run_id(&start(&x).await);
    let (s, b) = sa(
        &x,
        "POST",
        &format!("/api/platform/generator/runs/{id}/draft"),
        json!({"hint": "x".repeat(2001)}),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "hint_too_long"),
        "{b}"
    );
    let (s, b) = call(
        &x,
        "POST",
        &tpath(&x, 1, "/draft"),
        &jd(),
        json!({"hint": "x".repeat(2001)}),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "hint_too_long"),
        "{b}"
    );
    // within the limit and no provider configured: 503
    let (s, b) = sa(
        &x,
        "POST",
        &format!("/api/platform/generator/runs/{id}/draft"),
        json!({"hint": "ok"}),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::SERVICE_UNAVAILABLE, "unavailable"),
        "{b}"
    );
}

#[derive(Default)]
struct Seen {
    bodies: std::sync::Mutex<Vec<Value>>,
    calls: AtomicUsize,
}

async fn completions(State(s): State<Arc<Seen>>, Json(b): Json<Value>) -> Json<Value> {
    s.calls.fetch_add(1, Ordering::SeqCst);
    s.bodies.lock().unwrap().push(b);
    let content = json!({"items": [{"kind":"activity","title":"purchase:from_llm","detail":"d","payload":{"label":"From LLM"},"example":null,"new_object":null,"confidence":0.9}]}).to_string();
    Json(
        json!({"choices":[{"message":{"role":"assistant","content":content}}],"usage":{"prompt_tokens":100,"completion_tokens":20}}),
    )
}

async fn fake_llm() -> (String, Arc<Seen>) {
    let seen = Arc::new(Seen::default());
    let app = Router::new()
        .route("/v1/chat/completions", post(completions))
        .with_state(seen.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (format!("http://{addr}/v1"), seen)
}

#[tokio::test]
async fn hint_reaches_prompt_as_separate_message_and_is_not_in_audit_details() {
    let x = setup().await;
    let id = run_id(&start(&x).await);
    goto(&x, id, "journey").await;
    let (url, seen) = fake_llm().await;
    let d = OpenAiDrafter::direct(
        &url,
        "sk-test",
        "gpt-4.1",
        "prov-1",
        "platform_external",
        Duration::from_secs(10),
    )
    .unwrap();
    let hint = "focus on gift cards SECRET-HINT";
    let v = platform_draft(&x, id, Some(hint), &d).await.expect("draft");
    assert!(
        v["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["title"] == "purchase:from_llm"),
        "{v}"
    );
    let bodies = seen.bodies.lock().unwrap().clone();
    assert!(!bodies.is_empty());
    for b in &bodies {
        let m = b["messages"].as_array().unwrap();
        assert_eq!(m[0]["role"], "system");
        assert!(
            !m[0]["content"].as_str().unwrap().contains("SECRET-HINT"),
            "never in the system prompt"
        );
        assert!(
            !m[1]["content"].as_str().unwrap().contains("SECRET-HINT"),
            "never in the context message"
        );
        assert_eq!(m.len(), 3, "the hint is its own message: {b}");
        assert!(m[2]["content"].as_str().unwrap().contains("SECRET-HINT"));
    }
    let details: Vec<Value> = sqlx::query_scalar(
        "SELECT details FROM platform_audit WHERE action = 'generator.step_draft'",
    )
    .fetch_all(&x.e.owner)
    .await
    .unwrap();
    assert_eq!(details.len(), 1);
    assert!(
        !details[0].to_string().contains("SECRET-HINT"),
        "{}",
        details[0]
    );
    assert_eq!(details[0]["result"]["hint"], true);
    let stored: Option<String> = sqlx::query_scalar(
        "SELECT hint FROM generator_step WHERE run_id = $1 AND step = 'journey'",
    )
    .bind(id)
    .fetch_one(&x.e.owner)
    .await
    .unwrap();
    assert_eq!(stored.as_deref(), Some(hint));
    // metered to the platform-owned tenant, with a cost line per call
    let tenants: Vec<Uuid> =
        sqlx::query_scalar("SELECT DISTINCT tenant_id FROM usage_event WHERE source = 'generator'")
            .fetch_all(&x.e.owner)
            .await
            .unwrap();
    assert_eq!(tenants, vec![x.platform]);
}

#[tokio::test]
async fn draft_before_current_step_items_is_nothing_to_draft() {
    let x = setup().await;
    let id = run_id(&start(&x).await);
    goto(&x, id, "validate").await;
    let e = platform_draft(
        &x,
        id,
        None,
        &Fixed(vec![draft_item("activity", "purchase:a")]),
    )
    .await
    .expect_err("validate drafts nothing");
    assert_eq!(e.status_and_code().1, "nothing_to_draft");
    goto(&x, id, "journey").await;
    let e = platform_draft(&x, id, None, &Fixed(vec![]))
        .await
        .expect_err("no items");
    assert_eq!(e.status_and_code().1, "nothing_to_draft");
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM generator_item WHERE run_id = $1")
        .bind(id)
        .fetch_one(&x.e.owner)
        .await
        .unwrap();
    assert_eq!(n, 0);
}

// ---- T10b tenant runs --------------------------------------------------------------------------------------------------

#[tokio::test]
async fn tenant_step_without_job_designer_403_missing_role() {
    let x = setup().await;
    let id = tenant_run(&x, x.f.tenant, x.f.job.job_id, &x.f.job.job_key).await;
    let op = person("u-op", &[]);
    for (m, tail, body) in [
        ("POST", "/draft", json!({})),
        ("POST", "/items/x/decision", json!({"decision":"accepted"})),
        ("PUT", "/stages/purchase", json!({"out_of_scope": true})),
        ("POST", "/advance", json!(null)),
        ("POST", "/package", json!(null)),
    ] {
        let (s, b) = call(&x, m, &tpath(&x, id, tail), &op, body).await;
        assert_eq!(
            (s, code(&b)),
            (StatusCode::FORBIDDEN, "forbidden"),
            "{tail}: {b}"
        );
        assert_eq!(b["error"]["missing_role"], "job_designer", "{tail}: {b}");
    }
    // any member reads
    let (s, b) = send(&x.e.app, get(&tpath(&x, id, ""), Some(&op))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["scope"], "tenant");
}

#[tokio::test]
async fn other_tenant_other_job_and_platform_runs_are_404_run_not_found() {
    let x = setup().await;
    let other = design_contract::new_job(&x.e).await;
    let theirs = tenant_run(&x, other.tenant, other.job.job_id, &other.job.job_key).await;
    let (s, b) = send(&x.e.app, get(&tpath(&x, theirs, ""), Some(&jd()))).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "run_not_found"),
        "other tenant: {b}"
    );
    // another job of the SAME tenant
    let job2: Uuid = sqlx::query_scalar("INSERT INTO jobs (tenant_id, key, label, pins, created_by) VALUES ($1, 'second_job', 'Second', '[]', 'test') RETURNING id")
        .bind(x.f.tenant)
        .fetch_one(&x.e.owner)
        .await
        .unwrap();
    let second = tenant_run(&x, x.f.tenant, job2, "second_job").await;
    let (s, b) = send(&x.e.app, get(&tpath(&x, second, ""), Some(&jd()))).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "run_not_found"),
        "other job: {b}"
    );
    let (s, b) = call(
        &x,
        "POST",
        &tpath(&x, second, "/advance"),
        &jd(),
        json!(null),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "run_not_found"),
        "{b}"
    );
    let p = run_id(&start(&x).await);
    let (s, b) = send(&x.e.app, get(&tpath(&x, p, ""), Some(&jd()))).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "run_not_found"),
        "platform run: {b}"
    );
}

#[tokio::test]
async fn tenant_draft_unbound_generator_409_generator_provider_missing() {
    let x = setup().await;
    let id = tenant_run(&x, x.f.tenant, x.f.job.job_id, &x.f.job.job_key).await;
    let (s, b) = call(&x, "POST", &tpath(&x, id, "/draft"), &jd(), json!({})).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "generator_provider_missing"),
        "{b}"
    );
}

#[tokio::test]
async fn tenant_draft_meters_the_tenant_and_the_hint_is_not_audited() {
    let x = setup().await;
    let id = tenant_run(&x, x.f.tenant, x.f.job.job_id, &x.f.job.job_key).await;
    let (url, _seen) = fake_llm().await;
    let d = OpenAiDrafter::direct(
        &url,
        "sk-test",
        "gpt-4.1",
        "prov-1",
        "tenant_byo",
        Duration::from_secs(10),
    )
    .unwrap();
    let mut tx = x.f.job.begin(&x.e.state.db).await.unwrap();
    let run = drive::tenant_run(&mut tx, &x.f.job, id).await.unwrap();
    let v = drive::draft(
        &mut tx,
        &x.e.state,
        &run,
        &x.f.job.t.actor(),
        "u-jd",
        Some("TENANT-HINT".into()),
        &d,
    )
    .await
    .expect("draft");
    tx.commit().await.unwrap();
    assert!(
        v["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["title"] == "purchase:from_llm"),
        "{v}"
    );
    let usage: Vec<(Uuid, Option<Uuid>)> =
        sqlx::query_as("SELECT tenant_id, job_id FROM usage_event WHERE source = 'generator'")
            .fetch_all(&x.e.owner)
            .await
            .unwrap();
    assert!(
        !usage.is_empty()
            && usage
                .iter()
                .all(|u| *u == (x.f.tenant, Some(x.f.job.job_id))),
        "{usage:?}"
    );
    let details: Vec<Value> =
        sqlx::query_scalar("SELECT details FROM audit_event WHERE action = 'generator.step_draft'")
            .fetch_all(&x.e.owner)
            .await
            .unwrap();
    assert_eq!(details.len(), 1);
    assert!(!details[0].to_string().contains("TENANT-HINT"));
}

#[tokio::test]
async fn tenant_advance_writes_nothing_and_package_writes_job_draft_not_live() {
    let x = setup().await;
    let id = tenant_run(&x, x.f.tenant, x.f.job.job_id, &x.f.job.job_key).await;
    let layers_before: i64 = sqlx::query_scalar("SELECT count(*) FROM layer_row")
        .fetch_one(&x.e.owner)
        .await
        .unwrap();
    // an accepted new activity in a stage of the job
    let stage = {
        let (s, b) = send(
            &x.e.app,
            get(
                &format!("/api/jobs/{}/journey?of=draft", x.f.job.job_key),
                Some(&jd()),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        b["stages"][0]["key"].as_str().unwrap().to_owned()
    };
    sqlx::query("UPDATE generator_stage SET stage = $2, label = $2 WHERE run_id = $1")
        .bind(id)
        .bind(&stage)
        .execute(&x.e.owner)
        .await
        .unwrap();
    let key = format!("{stage}:tenant_probe");
    item(
        &x,
        id,
        "journey",
        "activity",
        &key,
        "accepted",
        json!({"label":"Tenant probe","actor":"customer"}),
        None,
    )
    .await;
    // journey -> objects -> validate -> use_cases -> package
    for expect in ["objects", "validate", "use_cases", "package"] {
        let (s, b) = call(&x, "POST", &tpath(&x, id, "/advance"), &jd(), json!(null)).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(b["current_step"], expect, "{b}");
    }
    let layers_after: i64 = sqlx::query_scalar("SELECT count(*) FROM layer_row")
        .fetch_one(&x.e.owner)
        .await
        .unwrap();
    assert_eq!(
        layers_before, layers_after,
        "tenant_advance_writes_nothing / tenant_package_never_writes_layers"
    );
    let (s, b) = call(&x, "POST", &tpath(&x, id, "/advance"), &jd(), json!(null)).await;
    assert_eq!((s, code(&b)), (StatusCode::CONFLICT, "use_tag"), "{b}");
    let (s, b) = call(&x, "POST", &tpath(&x, id, "/package"), &jd(), json!(null)).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["status"], "packaged");
    assert_eq!(b["tagged"], "refinements saved");
    // the job's draft now has the extension; nothing is live
    let (s, j) = send(
        &x.e.app,
        get(
            &format!("/api/jobs/{}/journey?of=draft", x.f.job.job_key),
            Some(&jd()),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{j}");
    assert!(
        j["activities"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["key"] == key),
        "{j}"
    );
    let (s, live) = send(
        &x.e.app,
        get(
            &format!("/api/jobs/{}/journey?of=live", x.f.job.job_key),
            Some(&jd()),
        ),
    )
    .await;
    if s == StatusCode::OK {
        assert!(
            !live["activities"]
                .as_array()
                .unwrap()
                .iter()
                .any(|a| a["key"] == key),
            "live is unchanged: {live}"
        );
    }
    let (s, b) = call(&x, "POST", &tpath(&x, id, "/package"), &jd(), json!(null)).await;
    assert_eq!((s, code(&b)), (StatusCode::CONFLICT, "run_packaged"), "{b}");
}

#[tokio::test]
async fn tenant_decide_success_path_records_the_decision_and_the_errors_hold() {
    let x = setup().await;
    let id = tenant_run(&x, x.f.tenant, x.f.job.job_id, &x.f.job.job_key).await;
    let it = item(
        &x,
        id,
        "journey",
        "activity",
        "purchase:probe",
        "pending",
        json!({}),
        None,
    )
    .await;
    let uri = tpath(&x, id, &format!("/items/{it}/decision"));
    let (s, b) = call(&x, "POST", &uri, &jd(), json!({"decision":"accepted"})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(run_id(&b), id, "the run view comes back: {b}");
    let stored: (String, Option<String>) = sqlx::query_as(
        "SELECT decision, decision_note FROM generator_item WHERE run_id = $1 AND item_id = $2",
    )
    .bind(id)
    .bind(&it)
    .fetch_one(&x.e.owner)
    .await
    .unwrap();
    assert_eq!(stored, ("accepted".to_owned(), None));
    // rename carries the note and the title follows it
    let (s, b) = call(
        &x,
        "POST",
        &uri,
        &jd(),
        json!({"decision":"renamed","note":"purchase:probe_two"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let title: String =
        sqlx::query_scalar("SELECT title FROM generator_item WHERE run_id = $1 AND item_id = $2")
            .bind(id)
            .bind(&it)
            .fetch_one(&x.e.owner)
            .await
            .unwrap();
    assert_eq!(title, "purchase:probe_two");
    let (s, b) = call(
        &x,
        "POST",
        &tpath(&x, id, "/items/no-such-item/decision"),
        &jd(),
        json!({"decision":"accepted"}),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "item_not_found"),
        "{b}"
    );
    let (s, b) = call(&x, "POST", &uri, &jd(), json!({"decision":"maybe"})).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    let (s, b) = call(&x, "POST", &uri, &jd(), json!({"decision":"merged"})).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "merge_target_required"),
        "{b}"
    );
    let later = item(
        &x,
        id,
        "use_cases",
        "use_case_card",
        "purchase:x",
        "pending",
        json!({}),
        None,
    )
    .await;
    let (s, b) = call(
        &x,
        "POST",
        &tpath(&x, id, &format!("/items/{later}/decision")),
        &jd(),
        json!({"decision":"accepted"}),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "step_not_reached"),
        "{b}"
    );
}

#[tokio::test]
async fn super_admin_in_tenant_drives_tenant_run() {
    let x = setup().await;
    let id = tenant_run(&x, x.f.tenant, x.f.job.job_id, &x.f.job.job_key).await;
    let mut r = common::json(
        "PUT",
        &tpath(&x, id, "/stages/purchase"),
        Some(&super_admin()),
        json!({"out_of_scope": true}),
    );
    r.headers_mut().insert(
        "x-active-tenant-id",
        x.f.tenant.to_string().parse().unwrap(),
    );
    let (s, b) = send(&x.e.app, r).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let out: bool = sqlx::query_scalar(
        "SELECT out_of_scope FROM generator_stage WHERE run_id = $1 AND stage = 'purchase'",
    )
    .bind(id)
    .fetch_one(&x.e.owner)
    .await
    .unwrap();
    assert!(out);
}

// ---- T10a evidence -----------------------------------------------------------------------------------------------------

fn multipart(name: &str, bytes: &str, cols: &[(&str, &str)]) -> (String, String) {
    let b = "XBOUNDARYX";
    let mut body = String::new();
    for (k, v) in cols {
        body.push_str(&format!(
            "--{b}\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n"
        ));
    }
    body.push_str(&format!("--{b}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{name}\"\r\nContent-Type: text/csv\r\n\r\n{bytes}\r\n--{b}--\r\n"));
    (format!("multipart/form-data; boundary={b}"), body)
}

async fn upload(x: &Fx, id: i64, name: &str, csv: &str) -> (StatusCode, Value) {
    let (ct, body) = multipart(name, csv, &[]);
    let req = axum::http::Request::builder()
        .method("POST")
        .uri(format!("/api/platform/generator/runs/{id}/evidence"))
        .header("authorization", format!("Bearer {}", super_admin()))
        .header("content-type", ct)
        .body(axum::body::Body::from(body))
        .unwrap();
    send(&x.e.app, req).await
}

#[tokio::test]
async fn evidence_only_on_platform_runs_and_needs_a_gates_binding() {
    let x = setup().await;
    let t = tenant_run(&x, x.f.tenant, x.f.job.job_id, &x.f.job.job_key).await;
    let (s, b) = upload(&x, t, "t.csv", "text\nwhere is my parcel\n").await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "not_a_platform_run"),
        "{b}"
    );
    let p = run_id(&start(&x).await);
    // no INTERNAL_LAYA_URL in this environment: the platform tenant has no gates provider
    let (s, b) = upload(&x, p, "t.csv", "text\nwhere is my parcel\n").await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "gates_provider_missing"),
        "{b}"
    );
}

#[derive(Default)]
struct LayaSeen {
    calls: AtomicUsize,
}

async fn laya(State(s): State<Arc<LayaSeen>>, Json(b): Json<Value>) -> Json<Value> {
    s.calls.fetch_add(1, Ordering::SeqCst);
    let msg = b["state"]["message"]
        .as_str()
        .unwrap_or_default()
        .to_lowercase();
    let keys: Vec<String> = b["questions"]["activity"]["criteria"]
        .as_object()
        .expect("criteria")
        .keys()
        .cloned()
        .collect();
    let choice = if msg.contains("lawn") {
        "none".to_owned()
    } else {
        keys.iter()
            .find(|k| *k != "none")
            .cloned()
            .unwrap_or_else(|| "none".into())
    };
    let probs: serde_json::Map<String, Value> = keys
        .iter()
        .map(|k| (k.clone(), json!(if *k == choice { 0.9 } else { 0.01 })))
        .collect();
    Json(
        json!({"model":"laya-test","answers":{"activity":{"type":"choice","choice":choice,"probabilities":probs,"confidence":0.9}},"usage":{"input_tokens":42,"output_tokens":3}}),
    )
}

async fn setup_laya() -> (Fx, Arc<LayaSeen>) {
    let platform = Uuid::new_v4();
    let seen = Arc::new(LayaSeen::default());
    let app = Router::new()
        .route("/v1/systemone", post(laya))
        .with_state(seen.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    let e = common::env_with(|v| {
        v.insert("AGENT_PLATFORM_TENANT_ID".into(), platform.to_string());
        v.insert("INTERNAL_LAYA_URL".into(), format!("http://{addr}"));
        v.insert("INTERNAL_LAYA_KEY".into(), "laya-token-evidence".into());
    })
    .await;
    insert_tenant(&e.owner, platform, "Platform").await;
    let f = journey_job(&e).await;
    (Fx { e, platform, f }, seen)
}

async fn wait_status(x: &Fx, ev: &str) -> String {
    for _ in 0..100 {
        let st: String = sqlx::query_scalar("SELECT status FROM generator_evidence WHERE id = $1")
            .bind(ev)
            .fetch_one(&x.e.owner)
            .await
            .unwrap();
        if st != "classifying" {
            return st;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    "classifying".into()
}

#[tokio::test]
async fn evidence_card_row_never_stored_and_classification_is_metered_to_platform_tenant() {
    let (x, seen) = setup_laya().await;
    let id = run_id(&start(&x).await);
    let csv = "text,count\nwhere is my parcel,3\nplease mow my lawn,5\nmy password is hunter2,9\n4111 1111 1111 1111 card,2\n";
    let (s, b) = upload(&x, id, "tickets.csv", csv).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let ev: String = sqlx::query_scalar("SELECT id FROM generator_evidence WHERE run_id = $1")
        .bind(id)
        .fetch_one(&x.e.owner)
        .await
        .unwrap();
    let texts: Vec<Option<String>> =
        sqlx::query_scalar("SELECT excerpt FROM generator_evidence_row WHERE evidence_id = $1")
            .bind(&ev)
            .fetch_all(&x.e.owner)
            .await
            .unwrap();
    assert_eq!(texts.len(), 2, "{texts:?}");
    assert!(
        texts
            .iter()
            .flatten()
            .all(|t| !t.contains("hunter2") && !t.contains("4111"))
    );
    assert_eq!(wait_status(&x, &ev).await, "classified");
    assert_eq!(
        seen.calls.load(Ordering::SeqCst),
        2,
        "one Laya call per distinct row"
    );
    let buckets: Vec<(String, String)> = sqlx::query_as(
        "SELECT excerpt, bucket FROM generator_evidence_row WHERE evidence_id = $1 ORDER BY ord",
    )
    .bind(&ev)
    .fetch_all(&x.e.owner)
    .await
    .unwrap();
    assert!(
        buckets
            .iter()
            .any(|(t, b)| t.contains("lawn") && b == "uncovered"),
        "{buckets:?}"
    );
    assert!(
        buckets
            .iter()
            .any(|(t, b)| t.contains("parcel") && b == "covered"),
        "{buckets:?}"
    );
    let metered: Vec<(Uuid, String)> = sqlx::query_as("SELECT DISTINCT tenant_id, use FROM usage_event WHERE source = 'generator' AND use LIKE 'evidence_%'").fetch_all(&x.e.owner).await.unwrap();
    assert_eq!(metered, vec![(x.platform, "evidence_classify".to_owned())]);
}

#[tokio::test]
async fn uncovered_evidence_cluster_is_rule7_info_not_red() {
    let (x, _seen) = setup_laya().await;
    let id = run_id(&start(&x).await);
    let csv = "text\nwhere is my parcel\nplease mow my lawn\n";
    let up = evidence::Upload {
        file_name: "t.csv".into(),
        format: agent_api::discovery::rows::Format::Csv,
        bytes: csv.as_bytes().to_vec(),
        text_column: None,
        count_column: None,
    };
    let (file, format, got) = evidence::collect(up).await.unwrap();
    // inserted without the request's spawned task, so the test runs the classifier itself with a fake drafter
    let mut tx = begin_platform(&x.e.state.db).await.unwrap();
    let run = drive::platform_run(&mut tx, id).await.unwrap();
    let ev = evidence::insert(
        &mut tx,
        &run,
        &Actor::system("service:test"),
        "u-super",
        &file,
        format,
        &got,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let (url, _s) = fake_llm_clusters().await;
    let d: Arc<dyn Drafter> = Arc::new(
        OpenAiDrafter::direct(
            &url,
            "sk",
            "gpt-4.1",
            "prov-1",
            "platform_external",
            Duration::from_secs(10),
        )
        .unwrap(),
    );
    assert_eq!(
        evidence::classify(&x.e.state, id, &ev, Some(d)).await,
        evidence::Outcome::Classified
    );
    let clusters: Value =
        sqlx::query_scalar("SELECT uncovered FROM generator_evidence WHERE id = $1")
            .bind(&ev)
            .fetch_one(&x.e.owner)
            .await
            .unwrap();
    assert_eq!(clusters[0]["label"], "Lawn care", "{clusters}");
    assert_eq!(clusters[0]["tickets"], 1);
    goto(&x, id, "validate").await;
    let (s, d) = send(
        &x.e.app,
        get(
            &format!("/api/platform/generator/runs/{id}"),
            Some(&super_admin()),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{d}");
    let r7: Vec<&Value> = d["findings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["rule"] == 7)
        .collect();
    assert_eq!(r7.len(), 1, "{d}");
    assert_eq!(r7[0]["severity"], "info", "{d}");
    // rule 7 never blocks: with every stage out of scope the run advances
    all_out_of_scope(&x, id).await;
    let (s, b) = sa(
        &x,
        "POST",
        &format!("/api/platform/generator/runs/{id}/advance"),
        json!(null),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

async fn clusters_completion(State(s): State<Arc<Seen>>, Json(b): Json<Value>) -> Json<Value> {
    s.calls.fetch_add(1, Ordering::SeqCst);
    let input: Value = serde_json::from_str(b["messages"][1]["content"].as_str().unwrap()).unwrap();
    let members: Vec<Value> = input["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["text"].as_str().unwrap_or_default().contains("lawn"))
        .map(|m| m["id"].clone())
        .collect();
    let content = json!({"items": [{"kind":"discovery_cluster","title":"Lawn","detail":"","payload":{"label":"Lawn care","members":members,"suggestion":{"kind":"new_activity","slug":"lawn_care","label":"Lawn care help"}},"example":null,"new_object":null,"confidence":0.8}]}).to_string();
    Json(
        json!({"choices":[{"message":{"role":"assistant","content":content}}],"usage":{"prompt_tokens":100,"completion_tokens":20}}),
    )
}

async fn fake_llm_clusters() -> (String, Arc<Seen>) {
    let seen = Arc::new(Seen::default());
    let app = Router::new()
        .route("/v1/chat/completions", post(clusters_completion))
        .with_state(seen.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (format!("http://{addr}/v1"), seen)
}
