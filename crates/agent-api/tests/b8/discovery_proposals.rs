//! B8 T15 (second half) — discovery proposals: `discoveryState` signals / proposals / usage / runs, accept (a D54
//! draft only) and reject. Real Postgres 18 through the HTTP router as `agent_app`; the import and its clusters are
//! seeded as the owner (T14b writes them in production). The traffic-signal proposals (hand-over, knowledge, route)
//! need case / grounding / call history and are covered by their lane's signal readers' tests.
use agent_api::design::{self, DesignOf, DraftRef, JobRef};
use axum::http::StatusCode;
use serde_json::{Value, json};

use crate::b8::discovery_import::{Fx, ok, refused, setup, uri};
use crate::common::{self, get, send};

const EXT: &str = "support:gift_cards";

/// A classified import with two clusters: `c1` (a new activity) and `c2` (the same key again, to collide).
async fn seed(x: &Fx) -> String {
    let id: String = sqlx::query_scalar(
        "INSERT INTO discovery_import (tenant_id, job_id, file, rows, distinct_rows, status, done, coverage, \
                                       uploaded_by) \
         VALUES ($1, $2, 'tickets.csv', 200, 100, 'classified', 100, 0.8, 'u-designer') RETURNING id",
    )
    .bind(x.f.tenant)
    .bind(x.f.job.job_id)
    .fetch_one(&x.e.owner)
    .await
    .unwrap();
    for (cid, label, tickets, slug) in [
        ("c1", "Gift cards", 40, "gift_cards"),
        ("c2", "Gift card balance", 12, "gift_cards"),
    ] {
        sqlx::query(
            "INSERT INTO discovery_cluster (tenant_id, job_id, import_id, id, label, tickets, examples, \
                                            suggestion, drafted_tokens) \
             VALUES ($1, $2, $3, $4, $5, $6, '[\"Where is my gift card?\"]'::jsonb, $7, $8)",
        )
        .bind(x.f.tenant)
        .bind(x.f.job.job_id)
        .bind(&id)
        .bind(cid)
        .bind(label)
        .bind(tickets)
        .bind(json!({"kind": "tenant_extension", "stage": "support", "slug": slug, "label": label}))
        .bind(if cid == "c1" { 120 } else { 0 })
        .execute(&x.e.owner)
        .await
        .unwrap();
    }
    id
}

fn pid(import: &str, cluster: &str) -> String {
    format!("p-cl-{import}-{cluster}")
}

async fn state(x: &Fx) -> Value {
    ok(send(&x.e.app, get(&uri(&x.f, ""), Some(&x.designer))).await)
}

async fn accept(x: &Fx, id: &str) -> (StatusCode, Value) {
    send(
        &x.e.app,
        common::json(
            "POST",
            &uri(&x.f, &format!("/proposals/{id}/accept")),
            Some(&x.designer),
            json!({}),
        ),
    )
    .await
}

async fn reject(x: &Fx, id: &str, body: Value) -> (StatusCode, Value) {
    send(
        &x.e.app,
        common::json(
            "POST",
            &uri(&x.f, &format!("/proposals/{id}/reject")),
            Some(&x.designer),
            body,
        ),
    )
    .await
}

fn find<'a>(s: &'a Value, id: &str) -> &'a Value {
    s["proposals"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == id)
        .unwrap_or_else(|| panic!("no proposal {id}: {}", s["proposals"]))
}

async fn live_value(x: &Fx, key: &str) -> Option<Value> {
    let jr: JobRef = x.f.job_ref();
    let mut r = DraftRef::new();
    r.insert("key".into(), Value::String(key.into()));
    let mut tx = x.f.job.begin(&x.e.state.db).await.unwrap();
    let v = design::value(
        &mut tx,
        &x.e.state.design,
        &jr,
        &DesignOf::Live,
        "activity",
        &r,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    v
}

async fn ledger_keys(x: &Fx) -> Vec<String> {
    sqlx::query_scalar("SELECT key FROM draft_ledger WHERE job_id = $1 ORDER BY key")
        .bind(x.f.job.job_id)
        .fetch_all(&x.e.owner)
        .await
        .unwrap()
}

#[tokio::test]
async fn discovery_proposals_from_clusters_and_signals() {
    let x = setup().await;
    let empty = state(&x).await;
    assert_eq!(empty["proposals"], json!([]));
    let import = seed(&x).await;
    let s = state(&x).await;
    let p = find(&s, &pid(&import, "c1"));
    assert_eq!(p["kind"], "new_activity");
    assert_eq!(p["status"], "open");
    assert_eq!(p["activity"], EXT);
    assert_eq!(p["evidence"]["source"], "import");
    assert_eq!(p["evidence"]["count"], 40);
    assert_eq!(p["cluster"], "c1");
    assert_eq!(p["import_id"], import);
    assert_eq!(p["drafted"]["tokens"], 120);
    assert_eq!(p["change"]["page"], "J4");
    assert_eq!(p["change"]["keys"], json!([format!("journey:{EXT}")]));
    assert_eq!(
        s["proposals"].as_array().unwrap().len(),
        2,
        "{}",
        s["proposals"]
    );
    // ids are stable across reads
    assert_eq!(state(&x).await["proposals"], s["proposals"]);
    let sources: Vec<&str> = s["signals"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["source"].as_str().unwrap())
        .collect();
    assert_eq!(sources, ["import", "live", "knowledge_search", "shadow"]);
    assert_eq!(s["signals"][0]["active"], true);
    assert_eq!(s["signals"][0]["count"], 200);
    assert_eq!(s["usage"]["lines"], json!([]));
    assert_eq!(s["usage"]["tokens"], 0);
    assert_eq!(s["runs"], json!([]));
}

#[tokio::test]
async fn discovery_accept_writes_draft_only() {
    let x = setup().await;
    let import = seed(&x).await;
    let id = pid(&import, "c1");
    let s = ok(accept(&x, &id).await);
    let p = find(&s, &id);
    assert_eq!(p["status"], "draft");
    assert_eq!(p["decided_by"], "u-designer");
    // live unchanged; the draft carries the extension; the ledger has the row
    assert_eq!(live_value(&x, EXT).await, None);
    assert_eq!(ledger_keys(&x).await, [format!("journey:{EXT}")]);
    let kind: String = sqlx::query_scalar(
        "SELECT kind FROM job_overlay_row WHERE job_id = $1 AND object = 'activity' AND key = $2",
    )
    .bind(x.f.job.job_id)
    .bind(EXT)
    .fetch_one(&x.e.owner)
    .await
    .unwrap();
    assert_eq!(kind, "extension");
    // the cluster shows accepted on the import
    let c = s["imports"][0]["clusters"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == "c1")
        .unwrap();
    assert_eq!(c["state"], "accepted");
    assert_eq!(c["accepted_by"], "u-designer");
    let audit: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_event WHERE action = 'discovery.proposal_accept'",
    )
    .fetch_one(&x.e.owner)
    .await
    .unwrap();
    assert_eq!(audit, 1);
}

#[tokio::test]
async fn discovery_decided_twice_409() {
    let x = setup().await;
    let import = seed(&x).await;
    let id = pid(&import, "c1");
    ok(accept(&x, &id).await);
    refused(
        accept(&x, &id).await,
        StatusCode::CONFLICT,
        "already_decided",
    );
    refused(
        reject(&x, &id, json!({"reason": "no"})).await,
        StatusCode::CONFLICT,
        "already_decided",
    );
    // the same activity from another cluster is already in the job
    refused(
        accept(&x, &pid(&import, "c2")).await,
        StatusCode::CONFLICT,
        "already_in_job",
    );
}

#[tokio::test]
async fn discovery_reject_needs_reason() {
    let x = setup().await;
    let import = seed(&x).await;
    let id = pid(&import, "c1");
    for body in [json!({}), json!({"reason": "   "})] {
        refused(
            reject(&x, &id, body).await,
            StatusCode::UNPROCESSABLE_ENTITY,
            "reason_required",
        );
    }
    let s = ok(reject(&x, &id, json!({"reason": " B2B goes through accounting "})).await);
    let p = find(&s, &id);
    assert_eq!(p["status"], "rejected");
    assert_eq!(p["reason"], "B2B goes through accounting");
    assert_eq!(ledger_keys(&x).await, Vec::<String>::new());
    refused(
        accept(&x, &id).await,
        StatusCode::CONFLICT,
        "already_decided",
    );
}

#[tokio::test]
async fn discovery_unknown_proposal_404() {
    let x = setup().await;
    seed(&x).await;
    refused(
        accept(&x, "nope").await,
        StatusCode::NOT_FOUND,
        "proposal_not_found",
    );
    refused(
        reject(&x, "nope", json!({"reason": "x"})).await,
        StatusCode::NOT_FOUND,
        "proposal_not_found",
    );
}

#[tokio::test]
async fn discovery_other_tenant_404() {
    let x = setup().await;
    let import = seed(&x).await;
    let (f2, token2) = crate::b8::discovery_import::other(&x).await;
    let id = pid(&import, "c1");
    let uri2 = format!(
        "/api/jobs/{}/discovery/proposals/{id}/accept",
        f2.job.job_key
    );
    let (s, b) = send(
        &x.e.app,
        common::json("POST", &uri2, Some(&token2), json!({})),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    assert_eq!(b["error"]["code"], "proposal_not_found", "{b}");
    // and the other tenant's state shows none of ours
    let (s, b) = send(
        &x.e.app,
        get(
            &format!("/api/jobs/{}/discovery", f2.job.job_key),
            Some(&token2),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["proposals"], json!([]));
}

#[tokio::test]
async fn discovery_accept_needs_job_designer() {
    let x = setup().await;
    let import = seed(&x).await;
    crate::common::insert_user(&x.e.owner, "u-obs", x.f.tenant, "tenant_user").await;
    let obs = crate::common::person("u-obs", &[]);
    let (s, b) = send(
        &x.e.app,
        common::json(
            "POST",
            &uri(&x.f, &format!("/proposals/{}/accept", pid(&import, "c1"))),
            Some(&obs),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
}
