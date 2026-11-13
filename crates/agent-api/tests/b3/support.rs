//! B3's own tiny platform content, authored through the platform routes as super_admin (no boot seeder). The
//! knowledge-kind tests need a job template whose stages ask for knowledge kinds: [`template`] authors them once per
//! database on top of `common::platform::offering` and returns the template (`job/customer_care_agent@v5`) that
//! pins them (the offering's `@v4` has no kind rows).
//!
//! Content (identifiers, not product content): `base` stages `support` and `exit`; the department layer's
//! `stage_knowledge_kind` rows `support|policy.returns` (required, store `faq-policies`), `support|policy.warranty`
//! and `exit|policy.privacy` (both optional).
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use axum::http::StatusCode;
use serde_json::{Value, json};
use tokio::sync::OnceCell;

use crate::common::platform::{self, DEPARTMENT_LAYER, INDUSTRY_LAYER, JOB_LAYER};
use crate::common::{TestEnv, json as req, send, super_admin};

async fn sa(e: &TestEnv, method: &str, uri: &str, body: Value) -> (StatusCode, Value) {
    send(&e.app, req(method, uri, Some(&super_admin()), body)).await
}

async fn add(e: &TestEnv, layer: &str, kind: &str, key: &str, fields: Value) {
    let id: String = sqlx::query_scalar("SELECT id::text FROM layer WHERE key = $1")
        .bind(layer)
        .fetch_one(&e.owner)
        .await
        .unwrap_or_else(|x| panic!("layer {layer}: {x}"));
    let (s, b) = sa(
        e,
        "POST",
        &format!("/api/layers/{id}/rows"),
        json!({"kind": kind, "key": key, "op": "add", "fields": fields}),
    )
    .await;
    let dup = s == StatusCode::CONFLICT && b["error"]["code"] == "duplicate_add";
    assert!(
        s == StatusCode::CREATED || dup,
        "add {kind} {key} to {layer}: {s} {b}"
    );
}

async fn tag(e: &TestEnv, layer: &str) {
    let (s, b) = sa(
        e,
        "POST",
        &format!("/api/platform/templates/{}/tags", layer.replace('/', ".")),
        json!({}),
    )
    .await;
    let nothing = s == StatusCode::CONFLICT && b["error"]["code"] == "nothing_to_tag";
    assert!(s == StatusCode::CREATED || nothing, "tag {layer}: {s} {b}");
}

type Cells = Mutex<HashMap<String, Arc<OnceCell<String>>>>;

fn cells() -> &'static Cells {
    static CELLS: OnceLock<Cells> = OnceLock::new();
    CELLS.get_or_init(Cells::default)
}

/// Author the knowledge-kind content (once per database) and return the template that pins it, as `layer@vN`.
pub async fn template(e: &TestEnv) -> String {
    platform::offering(e).await;
    let db: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&e.owner)
        .await
        .unwrap();
    let cell = cells()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .entry(db)
        .or_default()
        .clone();
    cell.get_or_init(|| build(e)).await.clone()
}

async fn build(e: &TestEnv) -> String {
    add(
        e,
        "base",
        "journey_stage",
        "support",
        json!({"label": "Support", "order": 1}),
    )
    .await;
    add(
        e,
        "base",
        "journey_stage",
        "exit",
        json!({"label": "Exit", "order": 2}),
    )
    .await;
    for (key, store, required) in [
        ("support|policy.returns", Some("faq-policies"), true),
        ("support|policy.warranty", Some("faq-policies"), false),
        ("exit|policy.privacy", Some("faq-policies"), false),
    ] {
        let label = key.split('|').nth(1).unwrap_or(key);
        add(
            e,
            DEPARTMENT_LAYER,
            "stage_knowledge_kind",
            key,
            json!({"label": label, "store_key": store, "required": required}),
        )
        .await;
    }
    for family in ["base", INDUSTRY_LAYER, DEPARTMENT_LAYER] {
        tag(e, family).await;
    }
    add(
        e,
        JOB_LAYER,
        "job_setting",
        "kinds_note",
        json!({"label": "Kinds note"}),
    )
    .await;
    tag(e, JOB_LAYER).await;
    let version: i32 = sqlx::query_scalar(
        "SELECT max(version) FROM job_template_tag WHERE layer = $1 AND status = 'tagged'",
    )
    .bind(JOB_LAYER)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    format!("{JOB_LAYER}@v{version}")
}
