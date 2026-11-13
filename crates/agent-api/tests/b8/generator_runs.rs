//! B8 T9a — generator run tables (0171), the super_admin reads (`generatorRuns`, `generatorRun`) and the usage
//! estimate (`estimateGeneratorRun`). Start (`startGeneratorRun`) is T9b (after B6b + KH): runs are written here by
//! the owner pool. Real Postgres 18, through the HTTP router as `agent_app`.
use axum::http::StatusCode;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::common::design_contract::{self, ContractJob};
use crate::common::{self, TestEnv, get, insert_tenant, insert_user, person, send, super_admin};

const DESIGNER: &str = "u-gen-designer";
const OBSERVER: &str = "u-gen-observer";

struct Fx {
    e: TestEnv,
    f: ContractJob,
}

async fn setup() -> Fx {
    let e = common::env().await;
    let f = design_contract::new_job(&e).await;
    insert_user(&e.owner, DESIGNER, f.tenant, "job_designer").await;
    insert_user(&e.owner, OBSERVER, f.tenant, "").await;
    Fx { e, f }
}

fn code(b: &Value) -> &str {
    b["error"]["code"].as_str().unwrap_or("")
}

/// A run written as start would (owner pool): the 9 steps, the stages, optionally packaged with `tokens`.
async fn run(
    x: &Fx,
    tenant: Uuid,
    job: Option<Uuid>,
    status: &str,
    tokens: i64,
    stages: &[(&str, bool)],
) -> i64 {
    let scope = if job.is_some() { "tenant" } else { "platform" };
    let tagged = (status == "packaged").then_some("industry_dept/retail/customer_service@v2");
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO generator_run (tenant_id, scope, industry, department, job_key, job_id, status, current_step, \
                                    funding, started_by, tagged, output) \
         VALUES ($1, $2, 'retail', 'customer_service', 'retail_agent', $3, $4, 'journey', 'platform_external', \
                 $5, $6, ARRAY['industry_dept/retail/customer_service@v1']) RETURNING id",
    )
    .bind(tenant)
    .bind(scope)
    .bind(job)
    .bind(status)
    .bind(DESIGNER)
    .bind(tagged)
    .fetch_one(&x.e.owner)
    .await
    .unwrap();
    for (i, (s, out)) in stages.iter().enumerate() {
        sqlx::query(
            "INSERT INTO generator_stage (run_id, stage, ord, label, out_of_scope) VALUES ($1, $2, $3, $2, $4)",
        )
        .bind(id)
        .bind(s)
        .bind(i as i16)
        .bind(out)
        .execute(&x.e.owner)
        .await
        .unwrap();
    }
    if tokens > 0 {
        sqlx::query(
            "INSERT INTO generator_cost_line (run_id, step, provider_id, model, tokens, funding) \
             VALUES ($1, 'journey', 'internal', 'gpt-4.1', $2, 'platform_external')",
        )
        .bind(id)
        .bind(tokens)
        .execute(&x.e.owner)
        .await
        .unwrap();
    }
    id
}

async fn estimate(x: &Fx, tok: &str, body: Value) -> (StatusCode, Value) {
    let mut r = common::json("POST", "/api/generator/runs/estimate", Some(tok), body);
    r.headers_mut().insert(
        "x-active-tenant-id",
        x.f.tenant.to_string().parse().unwrap(),
    );
    send(&x.e.app, r).await
}

#[tokio::test]
async fn summary_status_blocked_when_red_and_detail_hydrates() {
    let x = setup().await;
    let id = run(
        &x,
        x.f.tenant,
        None,
        "running",
        0,
        &[("purchase", false), ("exit", true)],
    )
    .await;
    sqlx::query(
        "INSERT INTO generator_item (run_id, item_id, ord, step, kind, title, decision) \
         VALUES ($1::bigint, $1::text || '-journey-1', 0, 'journey', 'activity', 'exit:cancel', 'accepted')",
    )
    .bind(id)
    .execute(&x.e.owner)
    .await
    .unwrap();
    let sa = super_admin();
    let (s, b) = send(&x.e.app, get("/api/platform/generator/runs", Some(&sa))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let row = &b["items"][0];
    assert_eq!(row["id"], id);
    assert_eq!(row["status"], "blocked", "{row}");
    assert_eq!(
        row["red"], 1,
        "rule 4 on purchase (exit is out of scope): {row}"
    );
    assert_eq!(
        row["tenant_id"],
        Value::Null,
        "a platform run shows no tenant"
    );
    assert_eq!(row["started_by_name"], DESIGNER);
    assert_eq!(row["provisional_tags"], json!([]));
    let (s, d) = send(
        &x.e.app,
        get(&format!("/api/platform/generator/runs/{id}"), Some(&sa)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{d}");
    assert_eq!(d["stages"][1]["activities"], json!(["exit:cancel"]), "{d}");
    assert_eq!(d["findings"][0]["rule"], 4, "{d}");
    assert_eq!(d["items"][0]["id"], format!("{id}-journey-1"));
    let (s, b) = send(
        &x.e.app,
        get("/api/platform/generator/runs/999999", Some(&sa)),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "run_not_found"),
        "{b}"
    );
    let (s, b) = send(&x.e.app, get("/api/platform/generator/runs/abc", Some(&sa))).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "run_not_found"),
        "{b}"
    );
    let (s, b) = send(
        &x.e.app,
        get("/api/platform/generator/runs", Some(&person(DESIGNER, &[]))),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::FORBIDDEN, "super_admin_required"),
        "{b}"
    );
}

#[tokio::test]
async fn other_tenant_cannot_see_tenant_run() {
    let x = setup().await;
    let other = Uuid::new_v4();
    insert_tenant(&x.e.owner, other, &format!("Other {other}")).await;
    let id = run(&x, x.f.tenant, Some(x.f.job.job_id), "running", 0, &[]).await;
    let count = |tenant: Uuid| {
        let db = x.e.state.db.clone();
        async move {
            let mut tx = agent_api::tenancy::begin_scoped(&db, tenant).await.unwrap();
            let n: i64 = sqlx::query_scalar("SELECT count(*) FROM generator_run WHERE id = $1")
                .bind(id)
                .fetch_one(&mut *tx)
                .await
                .unwrap();
            tx.commit().await.unwrap();
            n
        }
    };
    assert_eq!(count(x.f.tenant).await, 1, "its tenant sees it");
    assert_eq!(count(other).await, 0, "another tenant gets nothing");
    let (s, b) = send(
        &x.e.app,
        get("/api/platform/generator/runs", Some(&super_admin())),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        b["items"][0]["tenant_id"],
        x.f.tenant.to_string(),
        "super_admin sees it: {b}"
    );
    assert_eq!(b["items"][0]["cost"]["attributed_to"], "tenant");
    // a child row takes its run's tenant, and a tenant cannot add one to another tenant's run
    let mut tx = agent_api::tenancy::begin_scoped(&x.e.state.db, other)
        .await
        .unwrap();
    let err = sqlx::query(
        "INSERT INTO generator_step (run_id, step, ord, status) VALUES ($1, 'input', 0, 'done')",
    )
    .bind(id)
    .execute(&mut *tx)
    .await
    .unwrap_err();
    assert!(common::sqlstate(&err).is_some(), "{err}");
}

#[tokio::test]
async fn estimate_uses_past_runs() {
    let x = setup().await;
    let a = run(&x, x.f.tenant, None, "packaged", 1_000_000, &[]).await;
    let b_id = run(&x, x.f.tenant, None, "packaged", 2_000_000, &[]).await;
    run(&x, x.f.tenant, None, "running", 5_000_000, &[]).await;
    let (s, b) = estimate(&x, &super_admin(), json!({"scope": "platform"})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["tokens_low"], 800_000);
    assert_eq!(b["tokens_high"], 2_500_000);
    assert_eq!(b["basis"], json!([a, b_id]));
    assert_eq!(b["attributed_to"], "platform");
    assert_eq!(b["funding"], "platform_external");
    assert_eq!(b["steps"].as_array().unwrap().len(), 9);
}

#[tokio::test]
async fn estimate_fallback_and_access() {
    let x = setup().await;
    let (s, b) = estimate(&x, &person(DESIGNER, &[]), json!({"scope": "tenant"})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        (b["tokens_low"].clone(), b["tokens_high"].clone()),
        (json!(80_000), json!(125_000))
    );
    assert_eq!(b["attributed_to"], "tenant");
    assert_eq!(b["funding"], "tenant_byo");
    assert_eq!(
        b["steps"],
        json!([
            "input",
            "journey",
            "objects",
            "validate",
            "use_cases",
            "package"
        ])
    );
    assert_eq!(b["llm_steps"], json!(["journey", "objects", "use_cases"]));
    assert_eq!(b["tenant_name"], format!("Contract {}", x.f.tenant));
    // tenant_admin_cannot_start_platform_run (the estimate has start's access)
    let (s, b) = estimate(&x, &person(DESIGNER, &[]), json!({"scope": "platform"})).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::FORBIDDEN, "super_admin_required"),
        "{b}"
    );
    // tenant_admin_other_tenant_forbidden
    let (s, b) = estimate(
        &x,
        &person(DESIGNER, &[]),
        json!({"scope": "tenant", "tenant_id": Uuid::new_v4().to_string()}),
    )
    .await;
    assert_eq!((s, code(&b)), (StatusCode::FORBIDDEN, "forbidden"), "{b}");
    let (s, b) = estimate(&x, &person(OBSERVER, &[]), json!({"scope": "tenant"})).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
    assert_eq!(b["error"]["missing_role"], "job_designer", "{b}");
    let (s, b) = estimate(
        &x,
        &super_admin(),
        json!({"scope": "tenant", "tenant_id": Uuid::new_v4().to_string()}),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "tenant_not_found"),
        "{b}"
    );
    let (s, b) = estimate(&x, &super_admin(), json!({"scope": "galaxy"})).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_input"),
        "{b}"
    );
}
