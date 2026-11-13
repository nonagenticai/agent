//! B8 T15 (shadow half) — shadow mode is a runtime flag, never a route row (Review Focus 5): start / stop, the B4
//! router hook ([`ShadowRoutes`]), the post-turn observation and the 30-day purge. Real Postgres 18; the routes as
//! `agent_app`, the hook and `observe` as `agent_runtime` (0173 grants).
use std::sync::Arc;

use agent_api::discovery::shadow::{self, Observation, ShadowRoutes};
use agent_api::runtime::routes::{DefaultRoutes, ResolvedTarget, RouteQuery, RouteResolver};
use agent_api::runtime::scope::Env;
use axum::http::StatusCode;
use serde_json::{Value, json};

use crate::b8::discovery_import::{Fx, ok, refused, setup, uri};
use crate::common;

async fn start(x: &Fx, env: &str) -> (StatusCode, Value) {
    common::send(
        &x.e.app,
        common::json(
            "POST",
            &uri(&x.f, "/shadow"),
            Some(&x.designer),
            json!({ "env": env }),
        ),
    )
    .await
}

async fn stop(x: &Fx) -> (StatusCode, Value) {
    common::send(
        &x.e.app,
        common::json(
            "DELETE",
            &uri(&x.f, "/shadow"),
            Some(&x.designer),
            Value::Null,
        ),
    )
    .await
}

fn query(x: &Fx, env: Env, op: &str) -> RouteQuery {
    RouteQuery {
        tenant: x.f.tenant,
        job_id: x.f.job.job_id,
        job_key: x.f.job.job_key.clone(),
        env,
        op: op.to_string(),
    }
}

/// Resolve `op` through the shadow hook on an `agent_runtime` connection scoped to the fixture's tenant.
async fn resolve(x: &Fx, env: Env, op: &str) -> ResolvedTarget {
    let rt = common::runtime_pool(&x.e.owner).await;
    let mut tx = agent_api::tenancy::begin_scoped(&rt, x.f.tenant)
        .await
        .unwrap();
    let routes = ShadowRoutes::new(Arc::new(DefaultRoutes::new()));
    let t = routes.resolve(&mut tx, &query(x, env, op)).await.unwrap();
    tx.commit().await.unwrap();
    t
}

async fn route_rows(x: &Fx) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM operation_route")
        .fetch_one(&x.e.owner)
        .await
        .unwrap()
}

#[tokio::test]
async fn start_and_stop_toggle_with_409s() {
    let x = setup().await;
    let v = ok(start(&x, "test").await);
    assert_eq!(v["shadow"]["active"], true, "{v}");
    assert_eq!(v["shadow"]["env"], "test");
    assert_eq!(
        v["shadow"]["started_by"],
        crate::common::design_contract::DESIGNER
    );
    refused(
        start(&x, "prod").await,
        StatusCode::CONFLICT,
        "shadow_active",
    );
    refused(
        start(&x, "staging").await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_env",
    );
    let v = ok(stop(&x).await);
    assert_eq!(v["shadow"]["active"], false);
    refused(stop(&x).await, StatusCode::CONFLICT, "shadow_inactive");
    let audit: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_event WHERE action IN ('discovery.shadow_start', 'discovery.shadow_stop')",
    )
    .fetch_one(&x.e.owner)
    .await
    .unwrap();
    assert_eq!(audit, 2);
}

#[tokio::test]
async fn shadow_writes_no_route_rows() {
    let x = setup().await;
    let before = route_rows(&x).await;
    ok(start(&x, "test").await);
    // an operation with no route: handed over to agent_cases while shadow mode is on, nothing written
    assert!(matches!(
        resolve(&x, Env::Test, "order.refund").await,
        ResolvedTarget::AgentCases
    ));
    assert_eq!(route_rows(&x).await, before);
    ok(stop(&x).await);
    assert_eq!(route_rows(&x).await, before);
    assert!(matches!(
        resolve(&x, Env::Test, "order.refund").await,
        ResolvedTarget::NotOffered
    ));
}

#[tokio::test]
async fn shadow_active_only_for_its_env() {
    let x = setup().await;
    ok(start(&x, "prod").await);
    let rt = common::runtime_pool(&x.e.owner).await;
    let mut tx = agent_api::tenancy::begin_scoped(&rt, x.f.tenant)
        .await
        .unwrap();
    assert!(
        shadow::active(&mut tx, x.f.tenant, x.f.job.job_id, "prod")
            .await
            .unwrap()
    );
    assert!(
        !shadow::active(&mut tx, x.f.tenant, x.f.job.job_id, "test")
            .await
            .unwrap()
    );
    tx.commit().await.unwrap();
    assert!(matches!(
        resolve(&x, Env::Test, "order.refund").await,
        ResolvedTarget::NotOffered
    ));
    assert!(matches!(
        resolve(&x, Env::Prod, "order.refund").await,
        ResolvedTarget::AgentCases
    ));
    // a built-in keeps its route
    assert!(matches!(
        resolve(&x, Env::Prod, "knowledge.search").await,
        ResolvedTarget::Builtin { .. }
    ));
}

#[tokio::test]
async fn observe_with_b8_stores_no_excerpt() {
    let x = setup().await;
    let rt = common::runtime_pool(&x.e.owner).await;
    let obs = |case_id: &'static str, text: Option<&'static str>| Observation {
        tenant: x.f.tenant,
        job: x.f.job.job_id,
        case_id,
        env: "test",
        stage: Some("delivery"),
        activity: Some("delivery:where_is_my_order"),
        labelled: true,
        text,
    };
    // off: nothing is written
    let mut tx = agent_api::tenancy::begin_scoped(&rt, x.f.tenant)
        .await
        .unwrap();
    assert!(
        !shadow::observe(&mut tx, &obs("CS-1", Some("hello")))
            .await
            .unwrap()
    );
    tx.commit().await.unwrap();
    ok(start(&x, "test").await);
    let mut tx = agent_api::tenancy::begin_scoped(&rt, x.f.tenant)
        .await
        .unwrap();
    assert!(shadow::observe(&mut tx, &obs("CS-2", None)).await.unwrap());
    assert!(
        shadow::observe(&mut tx, &obs("CS-3", Some("my card 4111 1111 1111 1111")))
            .await
            .unwrap()
    );
    assert!(
        shadow::observe(&mut tx, &obs("CS-4", Some("mail anna@example.com")))
            .await
            .unwrap()
    );
    tx.commit().await.unwrap();
    let got: Vec<(String, Option<String>)> =
        sqlx::query_as("SELECT case_id, excerpt FROM discovery_observation ORDER BY case_id")
            .fetch_all(&x.e.owner)
            .await
            .unwrap();
    assert_eq!(
        got,
        [
            ("CS-2".to_string(), None),
            ("CS-3".to_string(), None),
            ("CS-4".to_string(), Some("mail [email]".to_string())),
        ]
    );
    let v = ok(common::send(&x.e.app, common::get(&uri(&x.f, ""), Some(&x.designer))).await);
    assert_eq!(v["shadow"]["conversations"], 3, "{v}");
    assert_eq!(v["shadow"]["labelled"], 3);
}

#[tokio::test]
async fn purge_deletes_only_older_than_30d() {
    let x = setup().await;
    for (case, age) in [("CS-old", "31 days"), ("CS-new", "29 days")] {
        sqlx::query(
            "INSERT INTO discovery_observation (tenant_id, job_id, case_id, env, at) \
             VALUES ($1, $2, $3, 'test', now() - $4::interval)",
        )
        .bind(x.f.tenant)
        .bind(x.f.job.job_id)
        .bind(case)
        .bind(age)
        .execute(&x.e.owner)
        .await
        .unwrap();
    }
    assert_eq!(shadow::purge_all(&x.e.state.db).await.unwrap(), 1);
    let left: Vec<String> = sqlx::query_scalar("SELECT case_id FROM discovery_observation")
        .fetch_all(&x.e.owner)
        .await
        .unwrap();
    assert_eq!(left, ["CS-new"]);
}
