//! Test support: a job template whose pinned retail industry layer authors the business-rule slots RET-01, WAR-01 and
//! PAY-04 as `rule_slot` rows (0222) and one activity naming `knowledge.search`, `case.read` and `case.create`, so a
//! test of rule behaviour or of derived routes owns the content it asserts. The slot definitions
//! (`retail_rules.json`) are the ones the removed `seed/templates.json` carried: the pure evaluator
//! (`release::rules_eval`) decides by those rule ids, and the goldens name their examples.
//!
//! [`template`] extends the minimal platform offering (`common::platform::offering`): one `rule_slot` row per slot on
//! the industry layer, an industry tag, one new `job_setting` on the job layer and a job tag (v5) whose recorded stack
//! pins the new industry tag. Idempotent per database. A test module declares `mod rules_support;` beside
//! `mod common;`.
#![allow(dead_code)]

use std::sync::OnceLock;

use axum::http::StatusCode;
use serde_json::{Value, json};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::common::design_contract::{self, ContractJob};
use crate::common::platform;
use crate::common::{TestEnv, json as req, send, super_admin};

const RULES: &str = include_str!("retail_rules.json");

fn lock() -> &'static Mutex<()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(()))
}

async fn sa(e: &TestEnv, uri: &str, body: Value) -> (StatusCode, Value) {
    send(&e.app, req("POST", uri, Some(&super_admin()), body)).await
}

async fn layer_id(e: &TestEnv, key: &str) -> String {
    sqlx::query_scalar("SELECT id::text FROM layer WHERE key = $1")
        .bind(key)
        .fetch_one(&e.owner)
        .await
        .unwrap_or_else(|x| panic!("layer {key}: {x}"))
}

async fn tag(e: &TestEnv, layer: &str) {
    let (s, b) = sa(
        e,
        &format!("/api/platform/templates/{}/tags", layer.replace('/', ".")),
        json!({}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "tag {layer}: {b}");
}

/// The newest tagged version of the job layer.
async fn newest(e: &TestEnv) -> i32 {
    sqlx::query_scalar(
        "SELECT max(version) FROM job_template_tag WHERE layer = $1 AND status = 'tagged'",
    )
    .bind(platform::JOB_LAYER)
    .fetch_one(&e.owner)
    .await
    .unwrap()
}

/// The job template (layer, version) whose bundle carries RET-01, WAR-01, PAY-04 and the `support:track_order`
/// activity (so its draft requires the `knowledge.search`, `case.read` and `case.create` Production routes).
pub async fn template(e: &TestEnv) -> (String, i32) {
    let o = platform::offering(e).await;
    let _g = lock().lock().await;
    let rules: Vec<Value> = serde_json::from_str(RULES).expect("retail_rules.json");
    let authored: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM job_template_tag \
          WHERE layer = $1 AND version > $2 AND status = 'tagged')",
    )
    .bind(o.job_layer)
    .bind(o.version)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    if !authored {
        let industry = layer_id(e, o.industry).await;
        for r in &rules {
            let (s, b) = sa(
                e,
                &format!("/api/layers/{industry}/rows"),
                json!({"kind": "rule_slot", "key": r["key"], "op": "add", "fields": r["fields"]}),
            )
            .await;
            assert_eq!(s, StatusCode::CREATED, "rule_slot {}: {b}", r["key"]);
        }
        // one activity naming the operations the contract jobs route in Production (release/derive.rs derives the
        // bundle's `requires.routes` from the folded activities' operations)
        let (s, b) = sa(
            e,
            &format!("/api/layers/{industry}/rows"),
            json!({"kind": "activity", "key": "support:track_order", "op": "add",
                   "fields": {"label": "Track an order", "actor": "customer", "required": true,
                              "operations": [{"op": "knowledge.search", "required": true},
                                             {"op": "case.read", "required": true},
                                             {"op": "case.create", "required": true}]}}),
        )
        .await;
        assert_eq!(s, StatusCode::CREATED, "activity: {b}");
        tag(e, o.industry).await;
        // a job tag records the stack at tag time: it needs one new untagged row
        let job = layer_id(e, o.job_layer).await;
        let (s, b) = sa(
            e,
            &format!("/api/layers/{job}/rows"),
            json!({"kind": "job_setting", "key": "rules_note", "op": "add",
                   "fields": {"label": "Rules authored"}}),
        )
        .await;
        assert_eq!(s, StatusCode::CREATED, "{b}");
        tag(e, o.job_layer).await;
    }
    (o.job_layer.to_owned(), newest(e).await)
}

/// A new contract job (own tenant) created from [`template`].
pub async fn new_job(e: &TestEnv) -> ContractJob {
    let t = template(e).await;
    design_contract::new_job_for_on(e, Uuid::new_v4(), t).await
}
