//! B7 (T11–T13, D54/D69): the whole design contract (`common::design_contract::run_all_seeded`, items 1–7) for
//! B7's three kinds, one test per kind. Each fresh contract job is seeded first (owner pool):
//!
//! - `route`: Production routes for the three required operations of the contract selection
//!   (`knowledge.search`, `case.read`, `case.create{return}` on prod), so the job's cuts and applies pass the D67
//!   routing gate once it has a route table;
//! - `mapping_override`: the system `crm-1` the samples name (the 0094 foreign key);
//! - `tenant_template`: nothing (a template version is its own row).
//!
//! Real Postgres 18 (one container for this binary, a fresh database per test), through `agent_app` and the real
//! router.
use crate::common;

use agent_api::design::DraftRef;
use common::TestEnv;
use common::design_contract::{self, ContractJob};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};

fn r(v: Value) -> DraftRef {
    v.as_object().cloned().expect("a ref is a JSON object")
}

/// The required Production routes of the contract selection, routed to fallbacks that run.
fn seed_routes<'a>(e: &'a TestEnv, f: &'a ContractJob) -> BoxFuture<'a, ()> {
    Box::pin(design_contract::route_required_prod_ops(e, f))
}

/// The system the mapping-override samples name: a stub test_shop@3 on the test env.
fn seed_instance<'a>(e: &'a TestEnv, f: &'a ContractJob) -> BoxFuture<'a, ()> {
    Box::pin(async move {
        common::platform::publish_template(
            e,
            "test_shop",
            "3",
            &[common::platform::Op::star("order", "read")],
        )
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO connector_instance (tenant_id, id, label, template_scope, template_key, template_version, \
                                             env, base_url, stub, status, created_by) \
             VALUES ($1, 'crm-1', 'CRM', 'platform', 'test_shop', '3', 'test', 'stub://test_shop', true, \
                     'ok', 'test')",
        )
        .bind(f.tenant)
        .execute(&e.owner)
        .await
        .unwrap();
    })
}

#[tokio::test]
async fn route_full_contract() {
    let e = common::env().await;
    let link = json!({
        "object": "order",
        "verb": "read",
        "variant": "*",
        "env": "test",
        "target": {"kind": "deep_link", "url": "https://shop.example.com/orders"},
        "updated_by": "u-designer",
        "updated_at": "2026-10-20T10:00:00Z",
    });
    let cases = json!({
        "object": "case",
        "verb": "create",
        "variant": "return",
        "env": "prod",
        "target": {"kind": "agent_cases"},
        "updated_by": "u-designer",
        "updated_at": "2026-10-20T10:00:00Z",
    });
    design_contract::run_all_seeded(
        &e,
        "route",
        &[
            (
                r(json!({"object": "order", "verb": "read", "variant": "*", "env": "test"})),
                link,
            ),
            (
                r(json!({"object": "case", "verb": "create", "variant": "return", "env": "prod"})),
                cases,
            ),
        ],
        None,
        Some(seed_routes),
    )
    .await;
}

#[tokio::test]
async fn mapping_override_full_contract() {
    let e = common::env().await;
    let v = json!({
        "op": "order.read",
        "field": "status",
        "default_path": "$.status",
        "path": "$.data.state",
        "updated_by": "u-designer",
        "updated_at": "2026-10-20T10:00:00Z",
    });
    design_contract::run_all_seeded(
        &e,
        "mapping_override",
        &[(
            r(json!({"instance": "crm-1", "op": "order.read", "field": "status"})),
            v,
        )],
        Some("order.read status: template default"),
        Some(seed_instance),
    )
    .await;
}

#[tokio::test]
async fn tenant_template_full_contract() {
    let e = common::env().await;
    let v = json!({
        "key": "erp_desk",
        "version": "1",
        "label": "ERP desk",
        "kind": "generic",
        "base": "generic_rest@2",
        "transport": "rest",
        "auth_kind": "basic",
        "auth_params": {},
        "credential_schema": [],
        "status": "draft",
        "description": "Our order desk",
        "ops": [{
            "object": "order",
            "verb": "read",
            "variant": "*",
            "method": "GET",
            "path": "/orders/{{canonical.order_id}}",
            "request": "",
            "field_map": [{"path": "$.status", "field": "status"}],
            "status_map": [],
            "idempotency": {"kind": "none"},
        }],
    });
    design_contract::run_all_seeded(
        &e,
        "tenant_template",
        &[(r(json!({"key": "erp_desk", "version": "1"})), v)],
        Some("no such template"),
        None,
    )
    .await;
}
