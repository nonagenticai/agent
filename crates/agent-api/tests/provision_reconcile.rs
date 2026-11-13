//! P4 T8: the hourly provisioning reconciler (crate::provision::reconcile) against the strata fake: drift is recorded
//! in `provision_state.drift` and cleared when strata matches what agent pushed, nothing is ever written to strata,
//! nothing is queued, and suspended / archived tenants are skipped. Real Postgres 18 (one container for this binary).
use crate::common;

use agent_api::providers::slots;
use agent_api::provision::reconcile::run_once;
use common::strata_fake::{self, StrataFake, lock};
use common::{TestEnv, insert_tenant};
use serde_json::{Value, json};
use uuid::Uuid;

/// A strata tenant registration of `model` as the fake reports it.
fn cap(model: &str) -> Value {
    json!({"source": "tenant", "base_url": "https://llm.example/v1", "dialect": "openai",
           "model": model, "has_api_key": true})
}

/// strata_rag holds `embed` = `remote_model` for `tenant`.
fn strata_holds(fake: &StrataFake, tenant: Uuid, remote_model: &str) {
    lock(&fake.rag).tenant_providers.insert(
        tenant,
        json!({"embed": cap(remote_model), "rerank": {"source": "default"}, "chat": {"source": "default"}}),
    );
}

/// agent pushed `embed` = `model` at version 1.
async fn pushed(e: &TestEnv, tenant: Uuid, model: &str) {
    let fpr = slots::remote_fpr(Some(&cap(model)));
    sqlx::query(
        "INSERT INTO provider_slot_remote (tenant_id, slot, pushed_version, pushed_fpr, pushed_at, observed_fpr, observed_at) \
         VALUES ($1, 'embed', 1, $2, now(), $2, now())",
    )
    .bind(tenant)
    .bind(fpr)
    .execute(&e.owner)
    .await
    .unwrap();
}

async fn drift(e: &TestEnv, tenant: Uuid, target: &str) -> Option<Value> {
    sqlx::query_scalar::<_, Option<Value>>(
        "SELECT drift FROM provision_state WHERE tenant_id = $1 AND target = $2",
    )
    .bind(tenant)
    .bind(target)
    .fetch_optional(&e.owner)
    .await
    .unwrap()
    .flatten()
}

/// Every request either fake received that is not a GET.
fn writes(fake: &StrataFake) -> Vec<String> {
    fake.rag_requests()
        .into_iter()
        .chain(fake.ingest_requests())
        .filter(|r| r.method != "GET")
        .map(|r| format!("{} {}", r.method, r.path))
        .collect()
}

async fn world(tenants: &[Uuid]) -> (TestEnv, StrataFake) {
    let fake = strata_fake::start().await;
    let e = common::env_with(|v| fake.env(tenants, v)).await;
    for t in tenants {
        insert_tenant(&e.owner, *t, &format!("Reconcile {t}")).await;
    }
    (e, fake)
}

#[tokio::test]
async fn reconciler_records_drift_and_writes_nothing_remote() {
    let t = Uuid::new_v4();
    let (e, fake) = world(&[t]).await;
    pushed(&e, t, "bge-m3").await;
    strata_holds(&fake, t, "mistral-embed");
    let r = run_once(&e.state).await.unwrap();
    assert_eq!((r.tenants, r.drift_found, r.failed), (1, 1, 0), "{r:?}");
    let d = drift(&e, t, "strata_rag").await.expect("strata_rag drift");
    let entries = d.as_array().expect("ProvisionDrift[]");
    assert_eq!(entries.len(), 1, "{d}");
    assert_eq!(entries[0]["slot"], "embed");
    assert_eq!(entries[0]["local_version"], 1);
    assert_eq!(entries[0]["remote_version"], 2);
    assert!(entries[0]["detected_at"].is_string(), "{d}");
    assert_eq!(
        entries[0]["ours"],
        json!(slots::remote_fpr(Some(&cap("bge-m3"))))
    );
    assert_eq!(
        entries[0]["remote"],
        json!(slots::remote_fpr(Some(&cap("mistral-embed"))))
    );
    assert_eq!(drift(&e, t, "strata_ingest").await, None);
    // the slot list sees the same observation
    let observed: Option<String> = sqlx::query_scalar(
        "SELECT observed_fpr FROM provider_slot_remote WHERE tenant_id = $1 AND slot = 'embed'",
    )
    .bind(t)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(
        observed,
        Some(slots::remote_fpr(Some(&cap("mistral-embed"))))
    );
    // read-only remotely, and nothing queued
    assert!(fake.rag_requests().iter().any(|q| q.method == "GET"));
    assert_eq!(writes(&fake), Vec::<String>::new());
    let queued: i64 = sqlx::query_scalar("SELECT count(*) FROM provision_outbox")
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(queued, 0);
    // a second pass keeps the same drift (the observation time does not move)
    let again = run_once(&e.state).await.unwrap();
    assert_eq!(again.drift_found, 1);
    assert_eq!(drift(&e, t, "strata_rag").await, Some(d));
    assert_eq!(writes(&fake), Vec::<String>::new());
}

#[tokio::test]
async fn reconciler_clears_drift_when_aligned() {
    let t = Uuid::new_v4();
    let (e, fake) = world(&[t]).await;
    pushed(&e, t, "bge-m3").await;
    strata_holds(&fake, t, "mistral-embed");
    run_once(&e.state).await.unwrap();
    assert!(drift(&e, t, "strata_rag").await.is_some());
    // strata is put back to what agent pushed
    strata_holds(&fake, t, "bge-m3");
    let r = run_once(&e.state).await.unwrap();
    assert_eq!((r.drift_found, r.drift_cleared), (0, 1), "{r:?}");
    assert_eq!(drift(&e, t, "strata_rag").await, None);
    assert_eq!(writes(&fake), Vec::<String>::new());
}

#[tokio::test]
async fn reconciler_skips_suspended_and_archived() {
    let (active, suspended, archived) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let (e, fake) = world(&[active, suspended, archived]).await;
    for t in [active, suspended, archived] {
        pushed(&e, t, "bge-m3").await;
        strata_holds(&fake, t, "mistral-embed");
    }
    sqlx::query("UPDATE tenants SET status = 'suspended' WHERE id = $1")
        .bind(suspended)
        .execute(&e.owner)
        .await
        .unwrap();
    sqlx::query("UPDATE tenants SET status = 'archived', archived_at = now() WHERE id = $1")
        .bind(archived)
        .execute(&e.owner)
        .await
        .unwrap();
    let r = run_once(&e.state).await.unwrap();
    assert_eq!((r.tenants, r.drift_found, r.failed), (1, 1, 0), "{r:?}");
    assert!(drift(&e, active, "strata_rag").await.is_some());
    for t in [suspended, archived] {
        assert_eq!(drift(&e, t, "strata_rag").await, None);
        let asked = fake
            .rag_requests()
            .into_iter()
            .chain(fake.ingest_requests())
            .any(|q| q.tenant == Some(t));
        assert!(!asked, "strata was asked about skipped tenant {t}");
    }
}
