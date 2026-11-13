//! B4 T9a over B5's procedures: `probe_turn` plans with the source the orchestrator binary runs
//! (`DesignProcedures`), so a PUBLISHED tenant procedure for the qualified space is what the probe predicts — its
//! reads run. Fixture as `probe_turn.rs` (a job from
//! the newest `job/customer_care_agent`, the stub instance on `test_shop@1`, an OpenAI-compatible fake as
//! the tenant's internal `qualify` model); the procedure is written through the `procedure` design target.
use crate::common::platform;
use agent_api::design::{CaseDesign, DraftRef};
use agent_api::runtime::routes::DEMO_STUB_INSTANCE;
use agent_api::turn::probe::{self, ProbeOpts};
use axum::http::StatusCode;
use serde_json::{Value, json};

use crate::common::design_contract::{self, ContractJob};
use crate::common::{self, TestEnv, json as req, send};
use crate::redis_env as r;

const SPACE: &str = "retail:order-support:order_status";
/// What the fake `qualify` model answers: an order-status request for RET-01.
const ORDER_STATUS: &str =
    r#"{"context_space": "retail:order-support:order_status", "slots": {"order_id": "RET-01"}}"#;

async fn fake_llm(content: &'static str) -> String {
    let app = axum::Router::new().route(
        "/v1/chat/completions",
        axum::routing::post(move |axum::Json(_v): axum::Json<Value>| async move {
            axum::Json(json!({
                "choices": [{"message": {"role": "assistant", "content": content}}],
                "usage": {"prompt_tokens": 9, "completion_tokens": 5},
            }))
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(l, app).await.unwrap();
    });
    format!("http://{addr}/v1")
}

struct Fx {
    e: TestEnv,
    f: ContractJob,
}

async fn setup(name: &str) -> Fx {
    let url = fake_llm(ORDER_STATUS).await;
    let vars = r::api_vars(name).await;
    let e = common::env_with(|v| {
        v.extend(vars.clone());
        v.insert("INTERNAL_LLM_URL".into(), url);
    })
    .await;
    // the newest template: seed v8's `delivery:where_is_my_order` grants the two reads (older tags predate it, R2)
    let f = crate::b4::support::new_job(&e).await;
    crate::b4::support::route_prod(&e, &f).await;
    sqlx::query("UPDATE tenants SET platform_owned = true WHERE id = $1")
        .bind(f.tenant)
        .execute(&e.owner)
        .await
        .unwrap();
    platform::stub_shop(&e, f.tenant, DEMO_STUB_INSTANCE).await;
    crate::b4::support::route_instance(
        &e,
        &f,
        DEMO_STUB_INSTANCE,
        &[("order", "read"), ("delivery", "read")],
    )
    .await;
    let tok = design_contract::release_manager(&e, &f).await;
    let body =
        json!({"selection": design_contract::cuttable_selection(), "expected_updated_at": null});
    let (s, b) = send(
        &e.app,
        req(
            "PUT",
            &format!("/api/jobs/{}/target", f.job.job_key),
            Some(&tok),
            body,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    Fx { e, f }
}

async fn draft(fx: &Fx) -> CaseDesign {
    let draft_rev: i64 = sqlx::query_scalar("SELECT draft_rev FROM jobs WHERE id = $1")
        .bind(fx.f.job.job_id)
        .fetch_one(&fx.e.owner)
        .await
        .unwrap();
    CaseDesign::Draft { draft_rev }
}

fn step(number: u32, phase: u32, name: &str, op: &str, effect: &str) -> Value {
    json!({
        "step_number": number, "phase": phase, "name": name, "operation": op, "variant": null,
        "modality": if effect == "R" { "observe" } else { "do" }, "side_effect": effect,
        "identity_level": "2", "confirm_back": false, "gate": false, "args": {"order_id": "$order_id"},
    })
}

/// The probe plans the designed procedure of the qualified space, as the orchestrator does: `order.read` then
/// `delivery.read` (both inside `delivery:where_is_my_order`'s grant, which grants no write) run against the stub.
/// Mutation: with `StaticProcedures` the probe runs the builtin `order_status` (one `order.read`).
#[tokio::test]
async fn the_probe_plans_the_designed_procedure_the_orchestrator_runs() {
    let fx = setup("probe-designed").await;
    let mut at = DraftRef::new();
    at.insert("name".into(), json!("refund_fix"));
    design_contract::tracked_set(
        &fx.e,
        &fx.f,
        "procedure",
        &at,
        Some(json!([{
            "version": 1, "status": "PUBLISHED", "context_space": SPACE, "description": "Refund a payment",
            "reviewers": [], "released_in": null, "pddl_domain": null, "success_criteria": [],
            "tool_requirements": [],
            "steps": [step(1, 0, "order", "order.read", "R"), step(2, 1, "shipment", "delivery.read", "R")],
        }])),
    )
    .await;
    let design = draft(&fx).await;
    let p = probe::probe_turn_with(
        &fx.e.state,
        &fx.f.job.t,
        &fx.f.job_ref(),
        design,
        "Where is my order RET-01? I want my money back.",
        &ProbeOpts::new(&fx.e.state),
    )
    .await
    .unwrap_or_else(|e| panic!("probe: {:?}", e.status_and_code()));
    assert_eq!(p.ran, ["order.read", "delivery.read"], "{p:?}");
    assert!(p.would_call.is_empty(), "{p:?}");
}
