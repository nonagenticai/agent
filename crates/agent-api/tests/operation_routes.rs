//! B7 T13: job operations, routes, tools and routing readiness (`/api/jobs/{job}/operations…`, `/api/jobs/{job}/tools`)
//! on a real Postgres 18 with the booted platform seed: a job built from the latest tagged `job/customer_care_agent`
//! (its expected operations come from the pinned compile), systems inserted as fixtures, every refusal code of the
//! fixture's `setOperationRoute`, the audit row per write, the D67 cells, and tenant isolation.
//!
//! With B2 (D54/D62/D67): a route write runs inside `design::track` (the job's ledger row, `draft_rev` /
//! `previous_rev`), Production reads the current release snapshot (`live_routes` / `live_cells` only when they differ
//! from the draft; a job never released has no Production routes and is never routing-ready), and approve-with-apply
//! refuses a draft whose required Production routes do not run (`release_not_ready`).
use crate::common;
use crate::ops_world;

use std::collections::HashMap;

use agent_api::connectors::readiness;
use agent_api::connectors::routes_store::{self, DesignOf};
use agent_api::design::DraftRef;
use agent_api::tenancy::begin_scoped;
use axum::http::StatusCode;
use base64::Engine as _;
use common::design_contract::{self, ContractJob};
use common::platform::{Op, draft_template, publish_template};
use common::{TestEnv, get, insert_tenant, insert_user, json, person, send};
use serde_json::{Value, json};
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

/// A job `cca` on the latest tagged customer-care template; its id.
async fn insert_job(e: &TestEnv, t: Uuid) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO jobs (tenant_id, key, label, pins, template_layer, template_version, created_by) \
         SELECT $1, $2, 'Customer care', pins, layer, version, 'test' FROM job_template_tag \
          WHERE layer = 'job/customer_care_agent' AND status = 'tagged' ORDER BY version DESC LIMIT 1 \
         RETURNING id",
    )
    .bind(t)
    .bind(JOB)
    .fetch_one(&e.owner)
    .await
    .expect("a tagged customer-care template")
}

/// One instance row (owner pool: fixtures only).
async fn insert_instance(
    e: &TestEnv,
    t: Uuid,
    id: &str,
    key: &str,
    version: &str,
    env: &str,
    stub: bool,
) {
    sqlx::query(
        "INSERT INTO connector_instance (tenant_id, id, label, template_scope, template_key, template_version, env, \
                                         base_url, stub, status, created_by) \
         VALUES ($1, $2, $2, 'platform', $3, $4, $5, $6, $7, 'ok', 'test')",
    )
    .bind(t)
    .bind(id)
    .bind(key)
    .bind(version)
    .bind(env)
    .bind(if stub {
        format!("stub://{key}")
    } else {
        "https://shop.example.com".to_owned()
    })
    .bind(stub)
    .execute(&e.owner)
    .await
    .unwrap();
}

async fn green(e: &TestEnv, t: Uuid, instance: &str, op: &str) {
    sqlx::query(
        "INSERT INTO connector_contract_result (tenant_id, instance_id, op, env, side_effect, outcome, at, ran_by) \
         VALUES ($1, $2, $3, 'prod', 'R', 'green', now(), 'test')",
    )
    .bind(t)
    .bind(instance)
    .bind(op)
    .execute(&e.owner)
    .await
    .unwrap();
}

struct World {
    e: TestEnv,
    t: Uuid,
    job: Uuid,
    im: String,
    obs: String,
}

/// A tenant with an integrations manager and an observer, job `cca`, and three systems: `shop-prod` (test_shop@3,
/// prod), `shop-test` (test_shop@3 stub, test), `erp-prod` (test_erp@0 — a draft platform template — prod).
async fn world() -> World {
    let e = common::env_with(with_keys).await;
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, "Tenant a").await;
    insert_user(&e.owner, "u-im-a", t, "integrations_manager").await;
    insert_user(&e.owner, "u-obs-a", t, "tenant_user").await;
    ops_world::template(&e).await;
    let job = insert_job(&e, t).await;
    let ops = [
        Op::star("order", "read"),
        Op::star("order", "list"),
        Op::star("order", "cancel"),
        Op::star("offering", "list"),
        Op::star("case", "read"),
        Op::new("case", "create", "return"),
    ];
    publish_template(&e, "test_shop", "3", &ops).await.unwrap();
    draft_template(&e, "test_erp", "0", &ops).await.unwrap();
    insert_instance(&e, t, "shop-prod", "test_shop", "3", "prod", false).await;
    insert_instance(&e, t, "shop-test", "test_shop", "3", "test", true).await;
    insert_instance(&e, t, "erp-prod", "test_erp", "0", "prod", false).await;
    World {
        e,
        t,
        job,
        im: person("u-im-a", &[]),
        obs: person("u-obs-a", &[]),
    }
}

fn route_uri(op: &str, env: &str) -> String {
    let op = op.replace('{', "%7B").replace('}', "%7D");
    format!("/api/jobs/{JOB}/operations/{op}/routes/{env}")
}

async fn put_route(w: &World, op: &str, env: &str, target: Value) -> (StatusCode, Value) {
    send(
        &w.e.app,
        json(
            "PUT",
            &route_uri(op, env),
            Some(&w.im),
            json!({ "target": target }),
        ),
    )
    .await
}

fn code(b: &Value) -> &str {
    b["error"]["code"].as_str().unwrap_or("")
}

async fn audit_count(e: &TestEnv, action: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM audit_event WHERE action = $1")
        .bind(action)
        .fetch_one(&e.owner)
        .await
        .unwrap()
}

#[tokio::test]
async fn route_validations_match_fixture() {
    let w = world().await;
    let inst = |id: &str| json!({"kind": "instance", "instance_id": id});
    let cases: Vec<(&str, &str, Value, StatusCode, &str)> = vec![
        // not an expected operation of this job (an insurance op; retail customer care pins the IT desk since v5,
        // so credential.* IS expected here) / not an op key at all
        (
            "claim.create",
            "prod",
            inst("shop-prod"),
            StatusCode::NOT_FOUND,
            "operation_not_expected",
        ),
        (
            "not-a-key",
            "prod",
            inst("shop-prod"),
            StatusCode::NOT_FOUND,
            "operation_not_expected",
        ),
        // R operations need a route or a fallback
        (
            "order.read",
            "prod",
            json!({"kind": "not_offered"}),
            StatusCode::UNPROCESSABLE_ENTITY,
            "required_operation",
        ),
        (
            "order.read",
            "prod",
            inst("ghost-system"),
            StatusCode::NOT_FOUND,
            "instance_not_found",
        ),
        // a test system on a prod route (a stub is always a test system, so a stub on prod is env_mismatch first:
        // the fixture's `stub_not_allowed_in_prod` is unreachable behind the 0093 CHECK stub ⇒ env = 'test')
        (
            "order.read",
            "prod",
            inst("shop-test"),
            StatusCode::UNPROCESSABLE_ENTITY,
            "env_mismatch",
        ),
        (
            "order.read",
            "test",
            inst("shop-prod"),
            StatusCode::UNPROCESSABLE_ENTITY,
            "env_mismatch",
        ),
        // knowledge.search is built-in: refused for an instance before the template is even consulted
        (
            "knowledge.search",
            "prod",
            inst("shop-prod"),
            StatusCode::UNPROCESSABLE_ENTITY,
            "not_implemented",
        ),
        (
            "order.read",
            "prod",
            json!({"kind": "builtin"}),
            StatusCode::UNPROCESSABLE_ENTITY,
            "not_implemented",
        ),
        (
            "order.read",
            "prod",
            json!({"kind": "agent_cases"}),
            StatusCode::UNPROCESSABLE_ENTITY,
            "not_implemented",
        ),
        (
            "order.read",
            "prod",
            inst("erp-prod"),
            StatusCode::CONFLICT,
            "template_not_active",
        ),
        (
            "document.read{invoice}",
            "prod",
            json!({"kind": "deep_link", "url": "http://shop.example.com/inv"}),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_url",
        ),
        (
            "document.read{invoice}",
            "prod",
            json!({"kind": "deep_link", "url": "https://shop example.com"}),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_url",
        ),
        (
            "order.read",
            "staging",
            inst("shop-prod"),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_input",
        ),
    ];
    for (op, env, target, status, want) in cases {
        let (s, b) = put_route(&w, op, env, target.clone()).await;
        assert_eq!((s, code(&b)), (status, want), "{op}@{env} {target}: {b}");
    }
    assert_eq!(
        audit_count(&w.e, "connector.route.set").await,
        0,
        "a refusal writes nothing"
    );
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM operation_route")
        .fetch_one(&w.e.owner)
        .await
        .unwrap();
    assert_eq!(n, 0);

    // and the accepted shapes
    for (op, env, target) in [
        ("order.read", "prod", inst("shop-prod")),
        (
            "order.read",
            "test",
            json!({"kind": "instance", "instance_id": "shop-test", "detail": "sandbox"}),
        ),
        // the runtime runs knowledge.search itself (runtime::routes::builtin_family), and readiness requires it
        ("knowledge.search", "prod", json!({"kind": "builtin"})),
        (
            "case.create{return}",
            "prod",
            json!({"kind": "agent_cases"}),
        ),
        ("offering.list", "prod", json!({"kind": "not_offered"})),
        (
            "document.read{invoice}",
            "prod",
            json!({"kind": "deep_link", "url": " https://shop.example.com/inv "}),
        ),
    ] {
        let (s, b) = put_route(&w, op, env, target.clone()).await;
        assert_eq!(s, StatusCode::OK, "{op}@{env} {target}: {b}");
        let (now, before) = (b["draft_rev"].as_i64(), b["previous_rev"].as_i64());
        assert!(
            matches!((now, before), (Some(n), Some(p)) if n == p + 1),
            "a new route bumps the job's draft revision: {b}"
        );
        assert_eq!(b["route"]["env"], env);
    }
    let (_, b) = put_route(
        &w,
        "document.read{invoice}",
        "prod",
        json!({"kind": "deep_link", "url": " https://shop.example.com/inv "}),
    )
    .await;
    assert_eq!(
        b["route"]["target"],
        json!({"kind": "deep_link", "url": "https://shop.example.com/inv"}),
        "the deep link is stored trimmed"
    );
    assert_eq!(b["route"]["variant"], "invoice");
}

#[tokio::test]
async fn set_route_records_one_audit_row_and_upserts() {
    let w = world().await;
    let (s, b) = put_route(
        &w,
        "order.read",
        "prod",
        json!({"kind": "instance", "instance_id": "shop-prod"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        b["route"],
        json!({
            "object": "order", "verb": "read", "variant": "*", "env": "prod",
            "target": {"kind": "instance", "instance_id": "shop-prod"},
            "updated_by": "u-im-a", "updated_at": b["route"]["updated_at"],
        })
    );
    // the same op × env again replaces the row
    let (s, b) = put_route(
        &w,
        "order.read",
        "prod",
        json!({"kind": "instance", "instance_id": "shop-prod", "detail": "v2"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["route"]["target"]["detail"], "v2");
    let (s, _) = put_route(
        &w,
        "order.read",
        "test",
        json!({"kind": "instance", "instance_id": "shop-test"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT env, target_kind FROM operation_route WHERE job_id = $1 ORDER BY env",
    )
    .bind(w.job)
    .fetch_all(&w.e.owner)
    .await
    .unwrap();
    assert_eq!(
        rows,
        vec![
            ("prod".to_owned(), "instance".to_owned()),
            ("test".to_owned(), "instance".to_owned())
        ]
    );
    assert_eq!(audit_count(&w.e, "connector.route.set").await, 3);
    let (job, resource, details): (Option<String>, String, Value) = sqlx::query_as(
        "SELECT job, resource_id, details FROM audit_event WHERE action = 'connector.route.set' ORDER BY id LIMIT 1",
    )
    .fetch_one(&w.e.owner)
    .await
    .unwrap();
    assert_eq!(job.as_deref(), Some(JOB));
    assert_eq!(resource, "order.read");
    assert_eq!(details["draft"], "route", "{details}");
    assert_eq!(details["instance"], "shop-prod", "{details}");
    assert_eq!(details["env"], "prod", "{details}");
}

#[tokio::test]
async fn clear_missing_route_404() {
    let w = world().await;
    let (s, b) = send(
        &w.e.app,
        json(
            "DELETE",
            &route_uri("order.read", "prod"),
            Some(&w.im),
            json!({}),
        ),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "route_not_found"),
        "{b}"
    );
    put_route(
        &w,
        "order.read",
        "prod",
        json!({"kind": "instance", "instance_id": "shop-prod"}),
    )
    .await;
    let (s, b) = send(
        &w.e.app,
        json(
            "DELETE",
            &route_uri("order.read", "prod"),
            Some(&w.im),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["route"], Value::Null, "{b}");
    assert_eq!(
        b["draft_rev"].as_i64(),
        b["previous_rev"].as_i64().map(|p| p + 1),
        "clearing a route bumps the job's draft revision: {b}"
    );
    assert_eq!(audit_count(&w.e, "connector.route.clear").await, 1);
    let (s, b) = send(
        &w.e.app,
        json(
            "DELETE",
            &route_uri("order.read", "prod"),
            Some(&w.im),
            json!({}),
        ),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "route_not_found"),
        "{b}"
    );
}

#[tokio::test]
async fn job_operations_answer_the_fixture_shape_and_cells() {
    let w = world().await;
    green(&w.e, w.t, "shop-prod", "order.read").await;
    put_route(
        &w,
        "order.read",
        "prod",
        json!({"kind": "instance", "instance_id": "shop-prod"}),
    )
    .await;
    put_route(
        &w,
        "order.cancel",
        "prod",
        json!({"kind": "instance", "instance_id": "shop-prod"}),
    )
    .await;
    let (s, b) = send(
        &w.e.app,
        get(&format!("/api/jobs/{JOB}/operations"), Some(&w.obs)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    for k in [
        "job",
        "draft_rev",
        "ops",
        "routes",
        "stats",
        "instances",
        "implements",
        "draft_instances",
        "tested",
        "names",
        "contract",
        "cells",
    ] {
        assert!(b.get(k).is_some(), "missing {k}: {b}");
    }
    assert_eq!(b["job"], JOB);
    assert_eq!(b["stats"], json!([]), "B4 seam");
    assert!(
        b.get("live_routes").is_none() && b.get("live_cells").is_none(),
        "never released: no Production routes to differ: {b}"
    );
    let ops = b["ops"].as_array().unwrap();
    let find = |key: &str| {
        ops.iter()
            .find(|o| {
                let v = o["variant"].as_str().unwrap();
                let k = if v == "*" {
                    format!(
                        "{}.{}",
                        o["object"].as_str().unwrap(),
                        o["verb"].as_str().unwrap()
                    )
                } else {
                    format!(
                        "{}.{}{{{v}}}",
                        o["object"].as_str().unwrap(),
                        o["verb"].as_str().unwrap()
                    )
                };
                k == key
            })
            .cloned()
    };
    let order_read = find("order.read").expect("order.read is expected");
    assert_eq!(order_read["required"], true);
    assert_eq!(order_read["side_effect"], "R");
    assert_eq!(
        find("order.cancel").expect("order.cancel")["required"],
        false
    );
    assert!(
        find("case.create{return}").is_some(),
        "variants are expected"
    );
    assert_eq!(b["draft_instances"], json!(["erp-prod"]));
    assert_eq!(b["tested"]["shop-prod"], json!(["order.read"]));
    assert_eq!(b["names"]["u-im-a"], "u-im-a");
    assert!(
        b["implements"]["shop-prod"]
            .as_array()
            .unwrap()
            .contains(&json!("order.read"))
    );
    let cells = &b["cells"];
    assert_eq!(
        cells["order.read@prod"]["state"], "ready",
        "{}",
        cells["order.read@prod"]
    );
    assert_eq!(cells["order.read@prod"]["runs"], true);
    assert_eq!(cells["order.read@prod"]["instance"], "shop-prod");
    assert_eq!(cells["order.cancel@prod"]["state"], "untested");
    assert_eq!(cells["order.read@test"]["state"], "unrouted");
    assert_eq!(
        cells["order.read@test"]["suggested"],
        Value::Null,
        "shop-test is a stub and shop-prod is not a test system: nothing to suggest"
    );
    assert_eq!(
        cells["knowledge.search@prod"]["suggested"],
        json!({"kind": "builtin"})
    );
    // every op has both envs
    assert_eq!(cells.as_object().unwrap().len(), ops.len() * 2);
}

#[tokio::test]
async fn job_tools_lists_transitions() {
    let w = world().await;
    let (s, b) = send(
        &w.e.app,
        get(&format!("/api/jobs/{JOB}/tools"), Some(&w.obs)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let tools = b.as_array().expect("JobTool[]");
    assert!(!tools.is_empty());
    for t in tools {
        for k in [
            "object",
            "verb",
            "variant",
            "key",
            "description",
            "input_fields",
            "output_fields",
            "confirm_back",
            "idempotency",
            "transitions",
        ] {
            assert!(t.get(k).is_some(), "missing {k}: {t}");
        }
    }
    let read = tools
        .iter()
        .find(|t| t["key"] == "order.read")
        .expect("order.read");
    assert_eq!(
        read["idempotency"],
        Value::Null,
        "reads carry no idempotency"
    );
    assert_eq!(read["transitions"], json!([]));
    assert!(
        tools
            .iter()
            .any(|t| !t["transitions"].as_array().unwrap().is_empty()),
        "some write changes its object's status: {b}"
    );
    for t in tools {
        for tr in t["transitions"].as_array().unwrap() {
            assert_ne!(tr["from"], tr["to"], "{t}");
            assert_eq!(tr["object"], t["object"]);
        }
    }
}

#[tokio::test]
async fn tenant_user_cannot_route() {
    let w = world().await;
    let (s, b) = send(
        &w.e.app,
        json(
            "PUT",
            &route_uri("order.read", "prod"),
            Some(&w.obs),
            json!({"target": {"kind": "instance", "instance_id": "shop-prod"}}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
    assert_eq!(b["error"]["missing_role"], "integrations_manager");
    let (s, b) = send(
        &w.e.app,
        json(
            "DELETE",
            &route_uri("order.read", "prod"),
            Some(&w.obs),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
}

#[tokio::test]
async fn other_tenant_job_404() {
    let w = world().await;
    let other = Uuid::new_v4();
    insert_tenant(&w.e.owner, other, "Tenant b").await;
    insert_user(&w.e.owner, "u-im-b", other, "integrations_manager").await;
    let im_b = person("u-im-b", &[]);
    for uri in [
        format!("/api/jobs/{JOB}/operations"),
        format!("/api/jobs/{JOB}/tools"),
    ] {
        let (s, b) = send(&w.e.app, get(&uri, Some(&im_b))).await;
        assert_eq!(
            (s, code(&b)),
            (StatusCode::NOT_FOUND, "job_not_found"),
            "{uri}: {b}"
        );
    }
    let (s, b) = send(
        &w.e.app,
        json(
            "PUT",
            &route_uri("order.read", "prod"),
            Some(&im_b),
            json!({"target": {"kind": "instance", "instance_id": "shop-prod"}}),
        ),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "job_not_found"),
        "{b}"
    );
}

#[tokio::test]
async fn routing_readiness_lists_unrouted_required_prod_ops_and_live_fails_closed() {
    let w = world().await;
    green(&w.e, w.t, "shop-prod", "order.read").await;
    put_route(
        &w,
        "order.read",
        "prod",
        json!({"kind": "instance", "instance_id": "shop-prod"}),
    )
    .await;
    let mut tx = begin_scoped(&w.e.state.db, w.t).await.unwrap();
    let reg = w.e.state.design.clone();
    let r = readiness::routing(&mut tx, &reg, w.job, JOB, DesignOf::Draft)
        .await
        .unwrap();
    assert!(!r.routing_ready);
    let subjects: Vec<&str> = r.blocking.iter().map(|i| i.subject.as_str()).collect();
    assert!(subjects.contains(&"route:order.list@prod"), "{subjects:?}");
    assert!(
        !subjects.contains(&"route:order.read@prod"),
        "a green route runs: {subjects:?}"
    );
    assert!(
        !subjects.iter().any(|s| s.starts_with("route:order.cancel")),
        "optional ops never block: {subjects:?}"
    );
    assert!(r.blocking.iter().all(|i| i.code == "route_not_ready"));
    assert!(
        r.blocking
            .iter()
            .all(|i| i.href == format!("/admin/jobs/{JOB}/operations"))
    );

    let cells = readiness::cells(&mut tx, &reg, w.job, DesignOf::Draft)
        .await
        .unwrap();
    assert_eq!(cells["order.read@prod"].state, "ready");
    // never released: Production has no routes, no cells and is never ready (fail closed, never the draft rows)
    assert!(
        routes_store::effective(&mut tx, &reg, w.job, "prod", DesignOf::Live)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        routes_store::effective(&mut tx, &reg, w.job, "prod", DesignOf::Draft)
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(
        readiness::cells(&mut tx, &reg, w.job, DesignOf::Live)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        !readiness::routing(&mut tx, &reg, w.job, JOB, DesignOf::Live)
            .await
            .unwrap()
            .routing_ready
    );
    tx.commit().await.unwrap();
}

// ---- B2: the draft ledger, the release snapshot and the routing gate ------------------------------------------------

/// Job `cca` as a B2 contract job (tracked writes, publishing).
fn cca(w: &World) -> ContractJob {
    design_contract::contract_job(w.t, w.job, JOB)
}

async fn draft_rev(w: &World) -> i64 {
    sqlx::query_scalar("SELECT draft_rev FROM jobs WHERE id = $1")
        .bind(w.job)
        .fetch_one(&w.e.owner)
        .await
        .unwrap()
}

async fn ledger_rows(w: &World, key: &str) -> Vec<(String, String, String, Value)> {
    sqlx::query_as(
        "SELECT kind, area, page, ref FROM draft_ledger WHERE job_id = $1 AND key = $2 ORDER BY id",
    )
    .bind(w.job)
    .bind(key)
    .fetch_all(&w.e.owner)
    .await
    .unwrap()
}

async fn operations(w: &World) -> Value {
    let (s, b) = send(
        &w.e.app,
        get(&format!("/api/jobs/{JOB}/operations"), Some(&w.obs)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    b
}

#[tokio::test]
async fn set_route_records_job_ledger_row_and_bumps_draft_rev() {
    let w = world().await;
    let key = "route:knowledge.search:prod";
    let before = draft_rev(&w).await;
    let builtin = json!({"kind": "builtin"});
    let (s, b) = put_route(&w, "knowledge.search", "prod", builtin.clone()).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        (b["previous_rev"].as_i64(), b["draft_rev"].as_i64()),
        (Some(before), Some(before + 1)),
        "{b}"
    );
    assert_eq!(draft_rev(&w).await, before + 1);
    assert_eq!(
        ledger_rows(&w, key).await,
        vec![(
            "route".to_string(),
            "routes".to_string(),
            "J6".to_string(),
            json!({"object": "knowledge", "verb": "search", "variant": "*", "env": "prod"}),
        )]
    );

    // the same target again changes nothing Production would run: no bump, still one ledger row
    let (s, b) = put_route(&w, "knowledge.search", "prod", builtin).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        (b["previous_rev"].as_i64(), b["draft_rev"].as_i64()),
        (Some(before + 1), Some(before + 1)),
        "{b}"
    );
    assert_eq!(ledger_rows(&w, key).await.len(), 1);

    // clearing it puts the live value (no route) back: a bump, and the ledger row is dropped
    let (s, b) = send(
        &w.e.app,
        json(
            "DELETE",
            &route_uri("knowledge.search", "prod"),
            Some(&w.im),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["draft_rev"].as_i64(), Some(before + 2), "{b}");
    assert!(ledger_rows(&w, key).await.is_empty());
}

#[tokio::test]
async fn live_routes_present_only_when_different() {
    let w = world().await;
    let f = cca(&w);
    design_contract::publishable(&w.e, &f).await;
    green(&w.e, w.t, "shop-prod", "order.read").await;
    let (s, b) = put_route(
        &w,
        "order.read",
        "prod",
        json!({"kind": "instance", "instance_id": "shop-prod"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    design_contract::publish(&w.e, &f).await;
    let b = operations(&w).await;
    assert!(
        b.get("live_routes").is_none() && b.get("live_cells").is_none(),
        "Production = the draft: {b}"
    );

    // an unpublished change: the draft moves, Production keeps the release's routes
    let (s, b) = put_route(&w, "offering.list", "prod", json!({"kind": "not_offered"})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let b = operations(&w).await;
    let live = b["live_routes"]
        .as_array()
        .unwrap_or_else(|| panic!("live_routes once the draft differs: {b}"));
    assert!(!live.iter().any(|r| r["object"] == "offering"), "{b}");
    assert!(
        b["routes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["object"] == "offering"),
        "{b}"
    );
    assert_ne!(b["cells"]["offering.list@prod"]["state"], "unrouted", "{b}");
    assert_eq!(
        b["live_cells"]["offering.list@prod"]["state"], "unrouted",
        "{b}"
    );
    assert_eq!(b["live_cells"]["order.read@prod"]["state"], "ready", "{b}");

    // the read API B4/B5 use agrees: Production runs the snapshot, the test env the working rows
    let reg = w.e.state.design.clone();
    let mut tx = begin_scoped(&w.e.state.db, w.t).await.unwrap();
    let prod = routes_store::effective(&mut tx, &reg, w.job, "prod", DesignOf::Live)
        .await
        .unwrap();
    assert!(prod.iter().all(|r| r.object != "offering"), "{prod:?}");
    assert!(
        prod.iter().any(|r| r.object == "order" && r.verb == "read"),
        "{prod:?}"
    );
    let draft = routes_store::effective(&mut tx, &reg, w.job, "prod", DesignOf::Draft)
        .await
        .unwrap();
    assert!(draft.iter().any(|r| r.object == "offering"), "{draft:?}");
    assert!(
        readiness::routing(&mut tx, &reg, w.job, JOB, DesignOf::Live)
            .await
            .unwrap()
            .blocking
            .iter()
            .all(|i| i.subject != "route:order.read@prod")
    );
    tx.commit().await.unwrap();
}

#[tokio::test]
async fn release_not_ready_lists_unrouted_required_prod_ops() {
    let w = world().await;
    let f = cca(&w);
    // a route table that does not route the selection's required Production operations
    let (s, b) = put_route(&w, "offering.list", "prod", json!({"kind": "not_offered"})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    design_contract::write(
        &w.e,
        &f,
        "selection",
        &DraftRef::new(),
        Some(design_contract::cuttable_selection()),
    )
    .await;
    // D110: the required knowledge is bound, so routing is all that blocks
    design_contract::bind_required_knowledge(&w.e, &f).await;
    let tok = design_contract::release_manager(&w.e, &f).await;
    let (s, b) = design_contract::approve(&w.e, &f, &tok, true).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "release_not_ready"),
        "{b}"
    );
    let blocking = b["error"]["blocking"].as_array().expect("blocking items");
    let subjects: Vec<&str> = blocking
        .iter()
        .map(|i| i["subject"].as_str().unwrap_or(""))
        .collect();
    let mut subjects = subjects;
    subjects.sort_unstable();
    assert_eq!(
        subjects,
        [
            "case.create{return}@prod",
            "case.read@prod",
            "knowledge.search@prod"
        ],
        "{b}"
    );
    assert!(
        blocking.iter().all(|i| i["code"] == "route_not_ready"),
        "{b}"
    );
    let board_gaps = {
        let (s, b) = send(
            &w.e.app,
            get(
                &format!("/api/jobs/{JOB}/readiness/board?of=draft"),
                Some(&tok),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        b
    };
    let routes_area = board_gaps["areas"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["key"] == "routes")
        .cloned()
        .unwrap_or(Value::Null);
    assert_eq!(routes_area["status"], "gap", "{routes_area}");
    assert_eq!(
        (&routes_area["ready"], &routes_area["total"]),
        (&json!(0), &json!(3)),
        "{routes_area}"
    );

    // routed to fallbacks that run: the same approve-and-apply passes
    design_contract::publishable(&w.e, &f).await;
    let (s, b) = design_contract::approve(&w.e, &f, &tok, true).await;
    assert!(s == StatusCode::CREATED || s == StatusCode::OK, "{s}: {b}");
    assert_eq!(b["applied"], true, "{b}");
}

/// R5: routing is a hard publish blocker. A job that uses operations but has NO route row at all used to publish
/// (Production then answered every call `no_route`); approve-with-apply now refuses it, naming each unrouted op.
#[tokio::test]
async fn release_not_ready_without_any_route_row() {
    let w = world().await;
    let f = cca(&w);
    design_contract::write(
        &w.e,
        &f,
        "selection",
        &DraftRef::new(),
        Some(design_contract::cuttable_selection()),
    )
    .await;
    // D110: the required knowledge is bound, so routing is all that blocks
    design_contract::bind_required_knowledge(&w.e, &f).await;
    let tok = design_contract::release_manager(&w.e, &f).await;
    let (s, b) = design_contract::approve(&w.e, &f, &tok, true).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "release_not_ready"),
        "{b}"
    );
    let message = b["error"]["message"].as_str().unwrap_or("");
    for op in [
        "knowledge.search@prod",
        "case.read@prod",
        "case.create{return}@prod",
    ] {
        assert!(message.contains(op), "the refusal names {op}: {message}");
    }
    let blocking = b["error"]["blocking"].as_array().expect("blocking items");
    assert_eq!(blocking.len(), 3, "{b}");
    assert!(
        blocking.iter().all(|i| i["code"] == "route_not_ready"),
        "{b}"
    );

    // routed: the same approve-and-apply passes
    design_contract::publishable(&w.e, &f).await;
    let (s, b) = design_contract::approve(&w.e, &f, &tok, true).await;
    assert!(s == StatusCode::CREATED || s == StatusCode::OK, "{s}: {b}");
    assert_eq!(b["applied"], true, "{b}");
}

// ---- procedure-needed operations: routable, listed, and readiness-checked -------------------------------------------

/// A PUBLISHED tenant procedure of job `cca` whose one step calls `offering.search` (in the catalogue, not expected).
async fn publish_procedure_on_unexpected_op(w: &World) {
    sqlx::query(
        "INSERT INTO procedure_version (tenant_id, job_id, name, version, status, context_space, description, \
                                        steps, created_by) \
         VALUES ($1, $2, 'find_offer', 1, 'PUBLISHED', 'retail:order-support:returns', 'Find an offer', $3, 'test')",
    )
    .bind(w.t)
    .bind(w.job)
    .bind(json!([{
        "step_number": 1, "phase": 0, "name": "find", "operation": "offering.search", "variant": null,
        "modality": "observe", "side_effect": "R", "identity_level": "0", "confirm_back": false, "gate": false,
        "args": {}
    }]))
    .execute(&w.e.owner)
    .await
    .unwrap();
}

async fn release_readiness(w: &World) -> Value {
    let (s, b) = send(
        &w.e.app,
        get(
            &format!("/api/jobs/{JOB}/release-readiness?of=draft"),
            Some(&w.obs),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    b
}

fn has(items: &Value, code: &str, subject: &str) -> bool {
    items.as_array().is_some_and(|xs| {
        xs.iter()
            .any(|i| i["code"] == code && i["subject"] == subject)
    })
}

#[tokio::test]
async fn an_op_no_procedure_uses_and_the_job_does_not_expect_is_still_refused() {
    let w = world().await;
    let (s, b) = put_route(
        &w,
        "offering.search",
        "prod",
        json!({"kind": "deep_link", "url": "https://shop.example.com/offers"}),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    assert_eq!(code(&b), "operation_not_expected", "{b}");
}

#[tokio::test]
async fn a_procedure_needed_op_is_routable_listed_and_clears_readiness() {
    let w = world().await;
    publish_procedure_on_unexpected_op(&w).await;

    // listed as needed by a procedure, required
    let ops = operations(&w).await;
    let listed = ops["ops"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["object"] == "offering" && o["verb"] == "search")
        .unwrap_or_else(|| panic!("offering.search is listed: {ops}"))
        .clone();
    assert_eq!(listed["note"], "Needed by a procedure", "{listed}");
    assert_eq!(listed["required"], true, "{listed}");

    // unrouted: blocks prod, warns for the test env
    let r = release_readiness(&w).await;
    assert!(
        has(&r["blocking"], "route_not_ready", "offering.search@prod"),
        "{r}"
    );
    assert!(
        has(
            &r["warnings"],
            "test_route_not_ready",
            "offering.search@test"
        ),
        "{r}"
    );

    // a procedure-needed op cannot be switched off
    let (s, b) = put_route(
        &w,
        "offering.search",
        "test",
        json!({"kind": "not_offered"}),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(code(&b), "required_operation", "{b}");

    // routing it is accepted, in both envs, and then readiness no longer flags it
    for env in ["prod", "test"] {
        let (s, b) = put_route(
            &w,
            "offering.search",
            env,
            json!({"kind": "deep_link", "url": "https://shop.example.com/offers"}),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{env}: {b}");
    }
    let r = release_readiness(&w).await;
    assert!(
        !has(&r["blocking"], "route_not_ready", "offering.search@prod"),
        "{r}"
    );
    assert!(
        !has(
            &r["warnings"],
            "test_route_not_ready",
            "offering.search@test"
        ),
        "{r}"
    );
}
