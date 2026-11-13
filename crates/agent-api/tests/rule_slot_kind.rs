//! 0222: the `rule_slot` layer-row kind. A super_admin authors a business-rule slot on the offering's industry layer
//! (`POST /api/layers/{id}/rows`); once tagged, the LayerCompiler folds it into `bundle.rules` of a bundle pinned to
//! that tag, and a bundle pinned to the earlier tag has none. A row that breaks the kind's schema is refused.
use crate::common;

use agent_api::platform::templates::{self, TemplatePin};
use agent_api::release::compile::TemplateCompiler;
use axum::http::StatusCode;
use common::platform::{self, Offering};
use common::{TestEnv, json as req, send, super_admin};
use serde_json::{Value, json};

async fn sa(e: &TestEnv, method: &str, uri: &str, body: Value) -> (StatusCode, Value) {
    send(&e.app, req(method, uri, Some(&super_admin()), body)).await
}

fn slot_fields() -> Value {
    json!({
        "label": "Return window",
        "applies_to": ["order.return"],
        "inputs": ["order.delivered_at"],
        "outcome": "eligible | ineligible:RET-01",
        "statement": "Returns are accepted within {window_days} of delivery.",
        "values": [{"key": "window_days", "label": "Return window", "unit": "days",
                    "hint": "Days after delivery.", "suggested": null}]
    })
}

async fn pins_with_industry(e: &TestEnv, o: &Offering, industry_version: i32) -> Vec<TemplatePin> {
    let mut c = e.state.db.acquire().await.unwrap();
    let mut pins = templates::resolve_template(&mut c, o.job_layer, o.version)
        .await
        .unwrap_or_else(|x| panic!("{}: {x:?}", o.template()))
        .pins;
    for p in pins.iter_mut().filter(|p| p.layer == o.industry) {
        p.version = industry_version;
    }
    pins
}

#[tokio::test]
async fn a_rule_slot_authored_on_the_industry_layer_compiles_into_bundle_rules() {
    let e = common::env().await;
    let o = platform::offering(&e).await;
    let industry_id: String = sqlx::query_scalar("SELECT id::text FROM layer WHERE key = $1")
        .bind(o.industry)
        .fetch_one(&e.owner)
        .await
        .unwrap();
    let before: i32 = sqlx::query_scalar(
        "SELECT max(version) FROM layer_tag WHERE family = $1 AND NOT provisional",
    )
    .bind(o.industry)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    let rows = format!("/api/layers/{industry_id}/rows");

    // a row that breaks the kind is refused: unknown field, a statement naming a missing value, a lower-case key
    let mut unknown = slot_fields();
    unknown["bogus"] = json!(1);
    let (s, b) = sa(
        &e,
        "POST",
        &rows,
        json!({"kind": "rule_slot", "key": "RET-01", "op": "add", "fields": unknown}),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    let mut dangling = slot_fields();
    dangling["statement"] = json!("Within {nope}.");
    let (s, b) = sa(
        &e,
        "POST",
        &rows,
        json!({"kind": "rule_slot", "key": "RET-01", "op": "add", "fields": dangling}),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    let (s, b) = sa(
        &e,
        "POST",
        &rows,
        json!({"kind": "rule_slot", "key": "ret-01", "op": "add", "fields": slot_fields()}),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");

    // the real one, then a tag
    let (s, b) = sa(
        &e,
        "POST",
        &rows,
        json!({"kind": "rule_slot", "key": "RET-01", "op": "add", "fields": slot_fields()}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let (s, b) = sa(
        &e,
        "POST",
        &format!(
            "/api/platform/templates/{}/tags",
            o.industry.replace('/', ".")
        ),
        json!({}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let after: i32 = sqlx::query_scalar(
        "SELECT max(version) FROM layer_tag WHERE family = $1 AND NOT provisional",
    )
    .bind(o.industry)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert!(after > before, "{before} -> {after}");

    let sel = templates::resolve_template(
        &mut e.state.db.acquire().await.unwrap(),
        o.job_layer,
        o.version,
    )
    .await
    .unwrap()
    .default_selection;

    // pinned to the new tag: the slot is in the bundle, whole, with its slot layer
    let pins = pins_with_industry(&e, &o, after).await;
    let mut c = e.state.db.acquire().await.unwrap();
    let got = TemplateCompiler::compile(e.state.compiler.as_ref(), &mut c, &sel, &pins)
        .await
        .unwrap_or_else(|x| panic!("compile: {x:?}"));
    assert_eq!(got.bundle.rules.len(), 1, "{:?}", got.bundle.rules);
    let r = &got.bundle.rules[0];
    assert_eq!(r.id, "RET-01");
    assert_eq!(r.label, "Return window");
    assert_eq!(r.applies_to, ["order.return"]);
    assert_eq!(r.inputs, ["order.delivered_at"]);
    assert_eq!(r.outcome, "eligible | ineligible:RET-01");
    assert_eq!(
        r.statement.as_deref(),
        Some("Returns are accepted within {window_days} of delivery.")
    );
    // the layer that set the slot (its key; the pin names the version)
    assert_eq!(r.slot_layer.as_deref(), Some(o.industry));
    assert_eq!(r.values.len(), 1);
    assert_eq!(r.values[0].key, "window_days");
    assert_eq!(r.values[0].unit, "days");

    // pinned to the tag before: no slot (the tag is immutable)
    let old = pins_with_industry(&e, &o, before).await;
    let got = TemplateCompiler::compile(e.state.compiler.as_ref(), &mut c, &sel, &old)
        .await
        .unwrap_or_else(|x| panic!("compile: {x:?}"));
    assert!(got.bundle.rules.is_empty(), "{:?}", got.bundle.rules);
}
