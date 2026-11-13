//! The `tests` readiness area and the `dor_tests` line (crate::readiness::contract_tests) on a real Postgres 18: job
//! `cca` on the latest tagged customer-care template (`rules_support::template`: one activity needs `case.read`) with the cuttable selection, its required Production
//! operations routed (`case.read` to the prod system `shop-prod`, the others built-in / `agent_cases`), and the
//! line read from the B7 contract result of `case.read` on `shop-prod`: pending with none, green after a passing
//! one, failing after a failing latest one.
use crate::common;
use crate::rules_support;

use std::collections::HashMap;
use std::sync::Arc;

use agent_api::design::DraftRef;
use agent_api::readiness::board;
use agent_api::readiness::contract_tests::ContractTestsReadiness;
use agent_api::readiness::{
    self, AreaKey, AreaStatus, Evaluation, ReadOf, Readiness, ReadinessCode, ReadinessStatus,
};
use base64::Engine as _;
use common::design_contract::{self, ContractJob};
use common::platform::{Op, publish_template};
use common::{TestEnv, insert_tenant};
use uuid::Uuid;

const JOB: &str = "cca";

fn with_keys(vars: &mut HashMap<String, String>) {
    let key = base64::engine::general_purpose::STANDARD.encode([9u8; 32]);
    vars.insert(
        "TENANT_DATA_KEYS".to_string(),
        format!(r#"{{"k1":"{key}"}}"#),
    );
    vars.insert("TENANT_DATA_KEY_CURRENT".to_string(), "k1".to_string());
}

struct World {
    e: TestEnv,
    t: Uuid,
    f: ContractJob,
}

/// Tenant, job `cca` with the cuttable selection, system `shop-prod`, and every required Production op routed.
async fn world() -> World {
    let e = common::env_with(with_keys).await;
    // the latest tagged template: the one whose industry layer authors the activity that needs `case.read`
    rules_support::template(&e).await;
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, "Tenant a").await;
    let job: Uuid = sqlx::query_scalar(
        "INSERT INTO jobs (tenant_id, key, label, pins, template_layer, template_version, created_by) \
         SELECT $1, $2, 'Customer care', pins, layer, version, 'test' FROM job_template_tag \
          WHERE layer = 'job/customer_care_agent' AND status = 'tagged' ORDER BY version DESC LIMIT 1 \
         RETURNING id",
    )
    .bind(t)
    .bind(JOB)
    .fetch_one(&e.owner)
    .await
    .expect("a tagged customer-care template");
    // the platform template the prod system is pinned to (published through the platform route)
    publish_template(
        &e,
        "demo_retail",
        "3",
        &[Op::star("case", "read"), Op::star("case", "create")],
    )
    .await
    .expect("platform template demo_retail@3");
    sqlx::query(
        "INSERT INTO connector_instance (tenant_id, id, label, template_scope, template_key, template_version, env, \
                                         base_url, stub, status, created_by) \
         VALUES ($1, 'shop-prod', 'shop-prod', 'platform', 'demo_retail', '3', 'prod', 'https://shop.example.com', \
                 false, 'ok', 'test')",
    )
    .bind(t)
    .execute(&e.owner)
    .await
    .unwrap();
    let f = design_contract::contract_job(t, job, JOB);
    design_contract::write(
        &e,
        &f,
        "selection",
        &DraftRef::new(),
        Some(design_contract::cuttable_selection()),
    )
    .await;
    for (object, verb, kind, instance) in [
        ("knowledge", "search", "builtin", None),
        ("case", "read", "instance", Some("shop-prod")),
        ("case", "create", "agent_cases", None),
    ] {
        sqlx::query(
            "INSERT INTO operation_route (tenant_id, job_id, object, verb, variant, env, target_kind, instance_id, \
                                          updated_by) \
             VALUES ($1, $2, $3, $4, '*', 'prod', $5, $6, 'test')",
        )
        .bind(t)
        .bind(job)
        .bind(object)
        .bind(verb)
        .bind(kind)
        .bind(instance)
        .execute(&e.owner)
        .await
        .unwrap();
    }
    World { e, t, f }
}

/// The latest contract result of `case.read` on `shop-prod` (one row per instance × op).
async fn result(w: &World, outcome: &str) {
    sqlx::query(
        "INSERT INTO connector_contract_result (tenant_id, instance_id, op, env, side_effect, outcome, note, at, \
                                                ran_by) \
         VALUES ($1, 'shop-prod', 'case.read', 'prod', 'R', $2, '', now(), 'test') \
         ON CONFLICT (tenant_id, instance_id, op) DO UPDATE SET outcome = EXCLUDED.outcome, at = EXCLUDED.at",
    )
    .bind(w.t)
    .bind(outcome)
    .execute(&w.e.owner)
    .await
    .unwrap();
}

async fn evaluate(w: &World) -> Evaluation {
    let mut r = Readiness::default();
    r.register(Arc::new(ContractTestsReadiness));
    let st = &w.e.state;
    let jr = w.f.job_ref();
    let mut tx = w.f.job.begin(&st.db).await.unwrap();
    let checked = readiness::checked(&mut tx, st.compiler.as_ref(), &jr, ReadOf::Draft)
        .await
        .unwrap();
    assert!(
        checked
            .requires
            .routes
            .contains(&"case.read@prod".to_string()),
        "the cuttable selection requires case.read on prod: {:?}",
        checked.requires.routes
    );
    let ev = r
        .evaluate(&mut tx, &st.design, st.compiler.as_ref(), &checked)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    ev
}

fn dor_tests(ev: &Evaluation) -> (ReadinessStatus, String) {
    let s = ev
        .definition_of_ready
        .iter()
        .find(|s| s.key == "dor_tests")
        .expect("a dor_tests line");
    assert_eq!(s.label, "Contract tests");
    assert_eq!(s.href, format!("/admin/jobs/{JOB}/operations"));
    (s.status, s.detail.clone())
}

fn tests_area(ev: &Evaluation) -> (AreaStatus, u32, u32) {
    let a = ev
        .board
        .areas
        .iter()
        .find(|a| a.key == AreaKey::Tests)
        .expect("a tests area");
    (a.status, a.ready, a.total)
}

fn pending_warnings(ev: &Evaluation) -> usize {
    board::release_readiness(&ev.board, &ev.missing)
        .warnings
        .iter()
        .filter(|i| i.code == ReadinessCode::ContractTestsPending)
        .count()
}

#[tokio::test]
async fn dor_tests_pending_without_a_contract_result() {
    let w = world().await;
    let ev = evaluate(&w).await;
    assert_eq!(
        dor_tests(&ev),
        (
            ReadinessStatus::Todo,
            "1 not run yet: case.read on shop-prod".to_string()
        )
    );
    assert_eq!(tests_area(&ev), (AreaStatus::Todo, 0, 1));
    assert_eq!(
        pending_warnings(&ev),
        1,
        "a go-live warning, never blocking"
    );
    assert!(ev.missing.iter().all(|m| m.dependency != "tests"));
}

#[tokio::test]
async fn dor_tests_green_after_a_passing_result() {
    let w = world().await;
    result(&w, "green").await;
    let ev = evaluate(&w).await;
    assert_eq!(
        dor_tests(&ev),
        (ReadinessStatus::Done, "All green".to_string())
    );
    assert_eq!(tests_area(&ev), (AreaStatus::Ok, 1, 1));
    assert_eq!(pending_warnings(&ev), 0);
}

#[tokio::test]
async fn dor_tests_failing_after_a_failing_latest_result() {
    let w = world().await;
    result(&w, "green").await;
    result(&w, "red").await;
    let ev = evaluate(&w).await;
    assert_eq!(
        dor_tests(&ev),
        (
            ReadinessStatus::Blocked,
            "1 failing: case.read on shop-prod".to_string()
        )
    );
    assert_eq!(tests_area(&ev), (AreaStatus::Gap, 0, 1));
    assert_eq!(pending_warnings(&ev), 1);
}
