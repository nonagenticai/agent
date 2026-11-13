//! Console redesign T6 — the one agent status (`GET /api/jobs/{job}/status`, crate::readmodel::agent_status) and the
//! surfaces that must agree with it: the job overview's `on_bus`, the tenant overview's and the tenant home's job
//! `on_bus`. Real Postgres 18, the real router, a job created from the contract template; the bus is the shared test
//! Redis, or an address nothing listens on (Review Focus 1: a current release with the bus down is never "live").
use axum::http::StatusCode;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::common::design_contract::{self, ContractJob};
use crate::common::{self, TestEnv, send, super_admin};
use crate::redis_env as r;

struct Fx {
    e: TestEnv,
    f: ContractJob,
}

/// An env on the shared Redis.
async fn env_up(test: &str) -> TestEnv {
    let vars = r::api_vars(test).await;
    common::env_with(|v| v.extend(vars.clone())).await
}

/// An env whose Redis address answers nothing: the bus is down, the API still boots.
async fn env_down(test: &str) -> TestEnv {
    let addr = r::dead_addr().await;
    let prefix = r::prefix(test);
    common::env_with(|v| {
        v.insert("REDIS_URL".to_string(), format!("redis://{addr}"));
        v.insert("AGENT_PREFIX".to_string(), prefix.clone());
    })
    .await
}

/// A job created from the contract template, made publishable (no release yet).
async fn with_job(e: TestEnv) -> Fx {
    let f = design_contract::new_job(&e).await;
    Fx { e, f }
}

/// A job created from the B4 test template and NOT made publishable: its draft still has blocking gaps.
async fn with_unready_job(e: TestEnv) -> Fx {
    // the B4 test template's activities name operations no route serves on Production: blocking gaps
    let template = crate::b4::support::template(&e).await;
    let tenant = Uuid::new_v4();
    common::insert_tenant(&e.owner, tenant, &format!("Status {tenant}")).await;
    common::licence_active(&e.owner, tenant).await;
    let mut q = common::json(
        "POST",
        "/api/jobs",
        Some(&super_admin()),
        json!({"template": format!("{}@v{}", template.0, template.1)}),
    );
    q.headers_mut()
        .insert("x-active-tenant-id", tenant.to_string().parse().unwrap());
    let (s, b) = send(&e.app, q).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let key = b["key"].as_str().unwrap().to_string();
    let id: Uuid = sqlx::query_scalar("SELECT id FROM jobs WHERE tenant_id = $1 AND key = $2")
        .bind(tenant)
        .bind(&key)
        .fetch_one(&e.owner)
        .await
        .unwrap();
    let f = design_contract::contract_job(tenant, id, &key);
    Fx { e, f }
}

/// GET `uri` as a super_admin acting in the fixture's tenant.
async fn get(fx: &Fx, uri: &str) -> (StatusCode, Value) {
    let mut q = common::get(uri, Some(&super_admin()));
    q.headers_mut().insert(
        "x-active-tenant-id",
        fx.f.tenant.to_string().parse().unwrap(),
    );
    send(&fx.e.app, q).await
}

async fn ok(fx: &Fx, uri: &str) -> Value {
    let (s, b) = get(fx, uri).await;
    assert_eq!(s, StatusCode::OK, "{uri}: {b}");
    b
}

async fn status(fx: &Fx) -> Value {
    ok(fx, &format!("/api/jobs/{}/status", fx.f.job.job_key)).await
}

/// The job's current release, as the apply writes it: a `current` row and `jobs.current_release_id`.
async fn make_current(fx: &Fx, id: &str) {
    sqlx::query(
        r#"INSERT INTO job_release (tenant_id, job_id, release_id, hash, bundle_hash, bundle, selection, pins,
               provenance, requires, causes, state, approved_by, approved_at, released_by, released_at,
               snapshot_kinds)
           VALUES ($1, $2, $3, $4, $4, '{}', '{}', '[]', '{}', '{}', '{}', 'current', 'u-rm',
               now() - interval '3 days', 'u-rm', now() - interval '2 days', '{}')"#,
    )
    .bind(fx.f.tenant)
    .bind(fx.f.job.job_id)
    .bind(id)
    .bind(format!("sha256:{}", "c".repeat(64)))
    .execute(&fx.e.owner)
    .await
    .unwrap();
    sqlx::query("UPDATE jobs SET current_release_id = $2 WHERE id = $1")
        .bind(fx.f.job.job_id)
        .bind(id)
        .execute(&fx.e.owner)
        .await
        .unwrap();
}

#[tokio::test]
async fn status_draft_without_release() {
    let fx = with_unready_job(common::env().await).await;
    let s = status(&fx).await;
    assert_eq!(s["state"], "draft", "{s}");
    assert!(s["live_release"].is_null(), "{s}");
    assert_eq!(s["on_bus"], false, "{s}");
    assert!(
        s["next_release"]["blockers"].as_i64().unwrap() > 0,
        "the draft has blocking gaps: {s}"
    );
    assert_eq!(s["open_cases"]["agent"], 0, "{s}");
    assert!(s["resolved_by_agent_30d"].is_null(), "{s}");
    assert_eq!(s["tests"]["state"], "no_questions", "{s}");
    assert!(s["tests"]["release_id"].is_null(), "{s}");
    assert_eq!(s["tests"]["questions"], 0, "{s}");
}

#[tokio::test]
async fn status_live_when_applied_and_bus_up() {
    let fx = with_job(env_up("status-live").await).await;
    make_current(&fx, "cfg-2026-10-23.1").await;
    let s = status(&fx).await;
    assert_eq!(s["state"], "live", "{s}");
    assert_eq!(s["on_bus"], true, "{s}");
    assert_eq!(s["urgent"], 0, "{s}");
    assert_eq!(s["live_release"]["id"], "cfg-2026-10-23.1", "{s}");
    let label = s["live_release"]["label"].as_str().unwrap();
    assert!(label.ends_with(" release"), "a human date: {label}");
    assert!(!label.contains("cfg-"), "no raw id in the label: {label}");
    assert!(s["live_release"]["since"].is_string(), "{s}");
    assert_eq!(s["tests"]["state"], "no_questions", "{s}");
    assert_eq!(s["tests"]["release_id"], "cfg-2026-10-23.1", "{s}");
}

/// A question on the job; its id.
async fn add_question(fx: &Fx, n: usize) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO validation_question (tenant_id, job_id, question, expected, reference, created_by) \
         VALUES ($1, $2, $3, 'yes', 'policy.returns', 'u-rm') RETURNING id",
    )
    .bind(fx.f.tenant)
    .bind(fx.f.job.job_id)
    .bind(format!("Question {n}?"))
    .fetch_one(&fx.e.owner)
    .await
    .unwrap()
}

async fn add_result(fx: &Fx, rel: &str, q: Uuid, pass: bool) {
    sqlx::query(
        "INSERT INTO validation_result (tenant_id, job_id, release_id, question_id, response, verdict, reason, \
             judged_by, judged_at) \
         VALUES ($1, $2, $3, $4, 'a', $5, $6, 'judge', now())",
    )
    .bind(fx.f.tenant)
    .bind(fx.f.job.job_id)
    .bind(rel)
    .bind(q)
    .bind(if pass { "pass" } else { "rejected" })
    .bind(if pass { None } else { Some("incorrect") })
    .execute(&fx.e.owner)
    .await
    .unwrap();
}

async fn add_cut_run(fx: &Fx, rel: &str) {
    sqlx::query(
        "INSERT INTO validation_cut_run (tenant_id, job_id, release_id, ran_by) VALUES ($1, $2, $3, 'u-rm')",
    )
    .bind(fx.f.tenant)
    .bind(fx.f.job.job_id)
    .bind(rel)
    .execute(&fx.e.owner)
    .await
    .unwrap();
}

#[tokio::test]
async fn status_tests_not_run_then_passing_then_failing() {
    let fx = with_job(common::env().await).await;
    let rel = "cfg-2026-10-23.1";
    make_current(&fx, rel).await;
    let (q1, q2) = (add_question(&fx, 1).await, add_question(&fx, 2).await);
    let s = status(&fx).await;
    assert_eq!(s["tests"]["state"], "not_run", "{s}");
    assert_eq!(s["tests"]["questions"], 2, "{s}");
    assert_eq!(s["tests"]["pending"], 2, "{s}");
    assert!(s["tests"]["last_run_at"].is_null(), "{s}");

    add_cut_run(&fx, rel).await;
    add_result(&fx, rel, q1, true).await;
    add_result(&fx, rel, q2, true).await;
    let s = status(&fx).await;
    assert_eq!(s["tests"]["state"], "passing", "{s}");
    assert_eq!(s["tests"]["passed"], 2, "{s}");
    assert_eq!(s["tests"]["failed"], 0, "{s}");
    assert_eq!(s["tests"]["release_id"], rel, "{s}");
    assert!(s["tests"]["last_run_at"].is_string(), "{s}");

    sqlx::query("UPDATE validation_result SET verdict = 'rejected', reason = 'incorrect' WHERE question_id = $1")
        .bind(q2)
        .execute(&fx.e.owner)
        .await
        .unwrap();
    let s = status(&fx).await;
    assert_eq!(s["tests"]["state"], "failing", "{s}");
    assert_eq!(s["tests"]["passed"], 1, "{s}");
    assert_eq!(s["tests"]["failed"], 1, "{s}");
}

#[tokio::test]
async fn status_tests_running_while_a_grading_is_active() {
    let fx = with_job(common::env().await).await;
    let rel = "cfg-2026-10-23.1";
    make_current(&fx, rel).await;
    add_question(&fx, 1).await;
    sqlx::query(
        "INSERT INTO validation_grid_run (tenant_id, job_id, release_id, trigger, status, total, by) \
         VALUES ($1, $2, $3, 'manual', 'running', 1, 'u-rm')",
    )
    .bind(fx.f.tenant)
    .bind(fx.f.job.job_id)
    .bind(rel)
    .execute(&fx.e.owner)
    .await
    .unwrap();
    let s = status(&fx).await;
    assert_eq!(s["tests"]["state"], "running", "{s}");
}

#[tokio::test]
async fn status_live_with_issues_when_bus_down() {
    let fx = with_job(env_down("status-down").await).await;
    make_current(&fx, "cfg-2026-10-23.1").await;
    let s = status(&fx).await;
    assert_eq!(s["state"], "live_with_issues", "never live: {s}");
    assert_eq!(s["on_bus"], false, "{s}");
    assert!(s["urgent"].as_i64().unwrap() >= 1, "{s}");
    // the same fact is an urgent inbox item for the operator, on every page
    let inbox = ok(&fx, &format!("/api/inbox?job={}", fx.f.job.job_key)).await;
    let item = inbox["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["title"] == "Agent isn't answering customers")
        .unwrap_or_else(|| panic!("the bus item: {inbox}"));
    assert_eq!(item["severity"], "blocking", "{item}");
    assert_eq!(item["owner_role"], "operator", "{item}");
    assert_eq!(item["job"], fx.f.job.job_key.as_str(), "{item}");
}

#[tokio::test]
async fn status_paused_when_tenant_suspended() {
    let fx = with_job(common::env().await).await;
    make_current(&fx, "cfg-2026-10-23.1").await;
    sqlx::query("UPDATE tenants SET status = 'suspended', suspended_at = now() WHERE id = $1")
        .bind(fx.f.tenant)
        .execute(&fx.e.owner)
        .await
        .unwrap();
    // a super_admin may act in a suspended tenant
    let s = status(&fx).await;
    assert_eq!(s["state"], "paused", "{s}");
}

#[tokio::test]
async fn tenant_overview_job_on_bus_matches_status() {
    for (name, up) in [("status-tov-up", true), ("status-tov-down", false)] {
        let e = if up {
            env_up(name).await
        } else {
            env_down(name).await
        };
        let fx = with_job(e).await;
        let s = status(&fx).await;
        assert_eq!(s["on_bus"], up, "{s}");
        let o = ok(&fx, "/api/tenant/overview").await;
        assert_eq!(o["jobs"][0]["on_bus"], s["on_bus"], "{o}");
        let h = ok(&fx, "/api/tenant/home").await;
        assert_eq!(h["jobs"][0]["job"]["on_bus"], s["on_bus"], "{h}");
        let list = ok(&fx, "/api/jobs?view=board").await;
        assert_eq!(list["items"][0]["job"]["on_bus"], s["on_bus"], "{list}");
    }
}

#[tokio::test]
async fn overview_on_bus_matches_status() {
    for (name, up) in [("status-ov-up", true), ("status-ov-down", false)] {
        let e = if up {
            env_up(name).await
        } else {
            env_down(name).await
        };
        let fx = with_job(e).await;
        let s = status(&fx).await;
        let o = ok(&fx, &format!("/api/jobs/{}/overview", fx.f.job.job_key)).await;
        assert_eq!(o["on_bus"], s["on_bus"], "{o}");
        assert_eq!(o["on_bus"], up, "{o}");
    }
}
