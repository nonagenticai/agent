//! B6b T12 `gateLibraryUsage` (the B5 bucket): per library question, the tenant jobs whose pins hold it, with the
//! tenant's B5 refinements and the last-7-days verdicts (`null` without traffic). Cross-tenant reads the R3 way.
use axum::http::StatusCode;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::common::{TestEnv, get, insert_tenant, insert_user, person, send, super_admin};

const USAGE: &str = "/api/platform/gates/usage";

fn code(b: &Value) -> &str {
    b["error"]["code"].as_str().unwrap_or("")
}

async fn usage(e: &TestEnv) -> Vec<Value> {
    let (s, b) = send(&e.app, get(USAGE, Some(&super_admin()))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    b.as_array().expect("GateUsage[]").clone()
}

fn gate<'a>(list: &'a [Value], key: &str) -> &'a Value {
    list.iter()
        .find(|g| g["gate"] == key)
        .unwrap_or_else(|| panic!("gate {key} in the usage"))
}

#[tokio::test]
async fn gate_usage_needs_super_admin() {
    let e = crate::common::env().await;
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, "Tenant a").await;
    insert_user(&e.owner, "u-ta", t, "tenant_admin").await;
    let (s, b) = send(&e.app, get(USAGE, Some(&person("u-ta", &[])))).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
    assert_eq!(code(&b), "super_admin_required");
}

#[tokio::test]
async fn gate_usage_lists_every_library_gate() {
    let e = crate::common::env().await;
    let list = usage(&e).await;
    let mut keys: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT key FROM layer_head WHERE kind = 'gate_question' AND op <> 'tombstone'",
    )
    .fetch_all(&e.owner)
    .await
    .unwrap();
    keys.sort();
    let got: Vec<&str> = list.iter().map(|g| g["gate"].as_str().unwrap()).collect();
    assert_eq!(got, keys.iter().map(String::as_str).collect::<Vec<_>>());
    assert!(list.iter().all(|g| g["jobs"].is_array()));
}

#[tokio::test]
async fn gate_usage_counts_pinned_jobs_and_refinements() {
    let e = crate::common::env().await;
    crate::b6b::support::gates(&e).await;
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, "Tenant a").await;
    let version: i32 = sqlx::query_scalar(
        "SELECT max(version) FROM layer_tag WHERE family = 'base' AND NOT provisional",
    )
    .fetch_one(&e.owner)
    .await
    .unwrap();
    let job: Uuid = sqlx::query_scalar(
        "INSERT INTO jobs (tenant_id, key, label, pins, created_by) VALUES ($1, 'care', 'Care', $2, 'test') \
         RETURNING id",
    )
    .bind(t)
    .bind(json!([{"layer": "base", "version": version}]))
    .fetch_one(&e.owner)
    .await
    .unwrap();
    let tid = t.to_string();
    let job_use = |list: &[Value]| -> Value {
        Value::Array(
            gate(list, "GC")["jobs"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|j| j["tenant_id"] == tid.as_str())
                .cloned()
                .collect(),
        )
    };
    assert_eq!(
        job_use(&usage(&e).await),
        json!([{"tenant_id": t.to_string(), "tenant_name": "Tenant a", "job": "care", "job_label": "Care",
                "refinements": 0, "verdicts_7d": null}])
    );
    sqlx::query(
        "INSERT INTO job_gate_option_refinement (tenant_id, job_id, gate, option, label, base_hash, updated_by) \
         VALUES ($1, $2, 'GC', 'b6b_opt', 'Relabelled', 'sha256:x', 'test')",
    )
    .bind(t)
    .bind(job)
    .execute(&e.owner)
    .await
    .unwrap();
    assert_eq!(job_use(&usage(&e).await)[0]["refinements"], 1);
    // another tenant's job pinned on nothing asks nothing
    let other = Uuid::new_v4();
    insert_tenant(&e.owner, other, "Tenant b").await;
    sqlx::query(
        "INSERT INTO jobs (tenant_id, key, label, pins, created_by) VALUES ($1, 'idle', 'Idle', '[]', 'test')",
    )
    .bind(other)
    .execute(&e.owner)
    .await
    .unwrap();
    let list = usage(&e).await;
    assert!(
        list.iter()
            .flat_map(|g| g["jobs"].as_array().unwrap())
            .all(|j| j["tenant_id"] != other.to_string())
    );
}

#[tokio::test]
async fn gate_usage_counts_verdicts_in_the_seven_day_window() {
    let e = crate::common::env().await;
    crate::b6b::support::gates(&e).await;
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, "Tenant w").await;
    let version: i32 = sqlx::query_scalar(
        "SELECT max(version) FROM layer_tag WHERE family = 'base' AND NOT provisional",
    )
    .fetch_one(&e.owner)
    .await
    .unwrap();
    let job: Uuid = sqlx::query_scalar(
        "INSERT INTO jobs (tenant_id, key, label, pins, created_by) VALUES ($1, 'win', 'Win', $2, 'test') \
         RETURNING id",
    )
    .bind(t)
    .bind(json!([{"layer": "base", "version": version}]))
    .fetch_one(&e.owner)
    .await
    .unwrap();
    // two answers inside the window (now, 1 day old), one 8 days old: outside it
    for age_days in [0_i32, 1, 8] {
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO gate_evaluation (tenant_id, job_id, at, point, env, conversation_id, outcome, decision) \
             VALUES ($1, $2, now() - make_interval(days => $3), 'P1', 'prod', 'conv-w', 'pass', 'allow') \
             RETURNING id",
        )
        .bind(t)
        .bind(job)
        .bind(age_days)
        .fetch_one(&e.owner)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO gate_answer (evaluation_id, tenant_id, job_id, at, gate, answer, value, confidence, unsure) \
             VALUES ($1, $2, $3, now() - make_interval(days => $4), 'GC', 'x', 'x', 0.9, false)",
        )
        .bind(id)
        .bind(t)
        .bind(job)
        .bind(age_days)
        .execute(&e.owner)
        .await
        .unwrap();
    }
    let tid = t.to_string();
    let list = usage(&e).await;
    let mine: Vec<&Value> = gate(&list, "GC")["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|j| j["tenant_id"] == tid.as_str())
        .collect();
    assert_eq!(mine.len(), 1, "{mine:?}");
    assert_eq!(
        mine[0]["verdicts_7d"],
        json!(2),
        "the 8-day-old answer is outside the window: {mine:?}"
    );
}
