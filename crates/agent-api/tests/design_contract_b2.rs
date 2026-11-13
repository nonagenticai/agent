//! B2 T16: the whole design contract (`common::design_contract::run_all`, items 1–7) for each of B2's six kinds,
//! one line per kind, plus the T3 ruling `is_projection()` ⇔ `places()` is `None` over every registered kind. The
//! first sample of each kind is also the release sample (items 5–6), so it must cut: no bundle violation (the
//! selection sample is the contract's complete selection plus a second channel). Real Postgres 18 (one container for
//! this binary, a fresh database per test), through the RLS runtime role `agent_app` and the real router.
use crate::common;

use agent_api::design::DraftRef;
use common::design_contract::{self, cuttable_selection};
use serde_json::{Value, json};

fn r(v: Value) -> DraftRef {
    v.as_object().cloned().expect("a ref is a JSON object")
}

fn two_channels() -> Value {
    let mut s = cuttable_selection();
    s["dimensions"]["channel"] = json!(["chat", "whatsapp"]);
    s
}

#[tokio::test]
async fn languages_full_contract() {
    let e = common::env().await;
    design_contract::run_all(
        &e,
        "languages",
        &[(r(json!({})), json!(["en", "nl"]))],
        Some("none"),
    )
    .await;
}

#[tokio::test]
async fn setting_full_contract() {
    let e = common::env().await;
    let v = json!({"value": "Lumen & Loom Home customers", "base_hash": "sha256:0a"});
    design_contract::run_all(
        &e,
        "setting",
        &[(r(json!({"key": "actor"})), v)],
        Some("release value"),
    )
    .await;
}

#[tokio::test]
async fn overlay_full_contract() {
    let e = common::env().await;
    let v = json!({
        "key": "billing",
        "kind": "override",
        "item": {"key": "billing", "label": "Billing and invoices"},
        "base_hash": "sha256:0a",
        "content_hash": "sha256:0b",
        "updated_by": "u-designer",
        "updated_at": "2026-10-20T10:00:00Z",
    });
    let at = r(json!({"object": "context_space", "key": "billing"}));
    design_contract::run_all(&e, "overlay", &[(at, v)], Some("release value")).await;
}

#[tokio::test]
async fn rule_values_full_contract() {
    let e = common::env().await;
    let v = json!({"max_days": "30", "fee": null});
    design_contract::run_all(&e, "rule_values", &[(r(json!({"id": "RET-01"})), v)], None).await;
}

#[tokio::test]
async fn rule_passage_full_contract() {
    let e = common::env().await;
    let v = json!({"title": "Returns policy §2", "text": "Items may be returned within 30 days."});
    design_contract::run_all(
        &e,
        "rule_passage",
        &[(r(json!({"id": "RET-01"})), v)],
        Some("no passage"),
    )
    .await;
}

#[tokio::test]
async fn selection_full_contract() {
    let e = common::env().await;
    design_contract::run_all(&e, "selection", &[(r(json!({})), two_channels())], None).await;
}

/// T3 ruling: for every registered kind, `places()` is `None` exactly when `is_projection()`.
#[tokio::test]
async fn projection_flag_agrees_with_places() {
    let e = common::env().await;
    design_contract::projection_agrees_with_places(&e).await;
}
