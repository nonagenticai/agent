//! Tenant rule definitions are validated data and share the draft/release/rollback spine with other design.
use crate::common;

use agent_api::design::DesignOf;
use agent_api::runtime::{design::RuntimeDesign, runner::builtin};
use axum::http::StatusCode;
use common::{get, json as req, person, send};
use serde_json::{Value, json};

fn definition(limit: &str) -> Value {
    json!({
        "label": "Service credit limit", "applies_to": ["case.create"], "inputs": ["order.total"],
        "outcome": "eligible | ineligible:CREDIT-LIMIT", "values": [],
        "facts": [{"key":"order.total","label":"Order total","kind":"number"}],
        "evaluation": {"condition":{"kind":"compare","left":{"kind":"fact","key":"order.total"},"op":"lte","right":{"kind":"literal","value":limit}},
            "eligible_reason":"Within the service credit policy.","ineligible_reason":"Above the service credit limit."}
    })
}

#[tokio::test]
async fn authored_rule_validation_roles_and_dry_run() {
    let e = common::env().await;
    let f = common::design_contract::new_job(&e).await;
    let path = format!("/api/jobs/{}/rules", f.job.job_key);
    let token = common::design_contract::release_manager(&e, &f).await;
    let mut body = definition("100");
    body["id"] = json!("CREDIT-LIMIT");
    let (st, _) = send(&e.app, req("POST", &path, None, body.clone())).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    common::insert_user(&e.owner, "rule-observer", f.tenant, "tenant_user").await;
    let (st, _) = send(
        &e.app,
        req(
            "POST",
            &path,
            Some(&person("rule-observer", &[])),
            body.clone(),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    let mut bad = body.clone();
    bad["evaluation"]["condition"]["left"]["key"] = json!("order.undeclared");
    let (st, b) = send(&e.app, req("POST", &path, Some(&token), bad)).await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    let (st, b) = send(&e.app, req("POST", &path, Some(&token), body.clone())).await;
    assert_eq!(st, StatusCode::CREATED, "{b}");
    assert_eq!(b["tenant_authored"], true);
    let (st, _) = send(&e.app, req("POST", &path, Some(&token), body)).await;
    assert_eq!(st, StatusCode::CONFLICT);
    for (facts, outcome) in [
        (json!({"order.total":"75"}), "eligible"),
        (json!({"order.total":"125"}), "ineligible"),
        (json!({}), "escalate"),
        (json!({"order.total":"NaN"}), "escalate"),
    ] {
        let (st, b) = send(
            &e.app,
            req(
                "POST",
                &format!("{path}/CREDIT-LIMIT/evaluate"),
                Some(&token),
                json!({"facts":facts}),
            ),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        assert_eq!(b["outcome"], outcome, "{b}");
    }
    // The generic overlay endpoint must not offer a way to overwrite template rule definitions.
    let mut reserved = definition("100");
    reserved["id"] = json!("RET-01");
    let (st, _) = send(&e.app, req("POST", &path, Some(&token), reserved)).await;
    assert_eq!(st, StatusCode::CONFLICT);
    let (st, b) = send(&e.app, get(&path, Some(&token))).await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert_eq!(
        b["items"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["id"] == "CREDIT-LIMIT")
            .count(),
        1
    );
}

#[tokio::test]
async fn definition_is_frozen_for_production_and_restored_on_rollback() {
    let e = common::env().await;
    let f = common::design_contract::new_job(&e).await;
    common::design_contract::publishable(&e, &f).await;
    let path = format!("/api/jobs/{}/rules", f.job.job_key);
    let token = common::design_contract::release_manager(&e, &f).await;
    let mut body = definition("100");
    body["id"] = json!("CREDIT-LIMIT");
    let (st, b) = send(&e.app, req("POST", &path, Some(&token), body)).await;
    assert_eq!(st, StatusCode::CREATED, "{b}");
    let jr = f.job_ref();
    let mut tx = f.job.begin(&e.state.db).await.unwrap();
    let live = RuntimeDesign::load(&mut tx, &e.state.design, &jr, &DesignOf::Live)
        .await
        .unwrap();
    assert!(
        !live.rules.iter().any(|r| r.id == "CREDIT-LIMIT"),
        "a draft definition cannot enter Production"
    );
    tx.commit().await.unwrap();
    let rm = common::design_contract::release_manager(&e, &f).await;
    let first = common::design_contract::publish(&e, &f).await;
    let (st, b) = send(
        &e.app,
        req(
            "PUT",
            &format!("{path}/CREDIT-LIMIT/definition"),
            Some(&token),
            definition("50"),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{b}");
    let mut tx = f.job.begin(&e.state.db).await.unwrap();
    let live = RuntimeDesign::load(&mut tx, &e.state.design, &jr, &DesignOf::Live)
        .await
        .unwrap();
    let draft = RuntimeDesign::load(&mut tx, &e.state.design, &jr, &DesignOf::Draft)
        .await
        .unwrap();
    let input = json!({"order.total":75}).as_object().unwrap().clone();
    assert_eq!(
        builtin::rule(&live, "CREDIT-LIMIT", &input).output["eligible"],
        true
    );
    assert_eq!(
        builtin::rule(&draft, "CREDIT-LIMIT", &input).output["eligible"],
        false
    );
    tx.commit().await.unwrap();
    common::design_contract::cut(&e, &f, &rm, true).await;
    let apply = format!("/api/jobs/{}/releases/{first}/apply", f.job.job_key);
    let (st, b) = send(&e.app, req("POST", &apply, Some(&rm), json!({}))).await;
    assert_eq!(st, StatusCode::OK, "{b}");
    let mut tx = f.job.begin(&e.state.db).await.unwrap();
    for of in [DesignOf::Live, DesignOf::Draft] {
        let restored = RuntimeDesign::load(&mut tx, &e.state.design, &jr, &of)
            .await
            .unwrap();
        assert_eq!(
            builtin::rule(&restored, "CREDIT-LIMIT", &input).output["eligible"],
            true,
            "rollback restores {of:?}"
        );
    }
}

#[tokio::test]
async fn thresholds_delete_and_discard_use_the_same_design_lifecycle() {
    let e = common::env().await;
    let f = common::design_contract::new_job(&e).await;
    let token = common::design_contract::release_manager(&e, &f).await;
    let path = format!("/api/jobs/{}/rules", f.job.job_key);
    let mut body = definition("100");
    body["id"] = json!("CREDIT-THRESHOLD");
    body["values"] = json!([{"key":"limit","label":"Credit limit","unit":"number"}]);
    body["evaluation"]["condition"]["right"] = json!({"kind":"value","key":"limit"});
    let (st, b) = send(&e.app, req("POST", &path, Some(&token), body)).await;
    assert_eq!(st, StatusCode::CREATED, "{b}");
    assert_eq!(b["placeholder"], true);
    let eval_path = format!("{path}/CREDIT-THRESHOLD/evaluate");
    let facts = json!({"facts":{"order.total":"75"}});
    let (st, b) = send(&e.app, req("POST", &eval_path, Some(&token), facts.clone())).await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert_eq!(b["outcome"], "placeholder");
    let value_path = format!("{path}/CREDIT-THRESHOLD");
    let (st, _) = send(
        &e.app,
        req(
            "PUT",
            &value_path,
            Some(&token),
            json!({"values":{"limit":"NaN"}}),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
    let (st, b) = send(
        &e.app,
        req(
            "PUT",
            &value_path,
            Some(&token),
            json!({"values":{"limit":"100.50"}}),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{b}");
    common::design_contract::publish(&e, &f).await;
    let (st, b) = send(&e.app, req("DELETE", &value_path, Some(&token), json!({}))).await;
    assert_eq!(st, StatusCode::NO_CONTENT, "{b}");
    let (st, _) = send(&e.app, req("POST", &eval_path, Some(&token), facts.clone())).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    let discard = format!("/api/jobs/{}/draft", f.job.job_key);
    let (st, b) = send(&e.app, req("DELETE", &discard, Some(&token), json!({}))).await;
    assert_eq!(st, StatusCode::OK, "{b}");
    let (st, b) = send(&e.app, req("POST", &eval_path, Some(&token), facts)).await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert_eq!(b["outcome"], "eligible");
}
