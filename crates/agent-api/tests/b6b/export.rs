//! B6b T12f `exportPlatformSeed` (F-30): `?family=` / `&tag=` narrow the export; it never writes a row (one
//! `platform.export` audit row only).
use axum::http::StatusCode;
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

use crate::common::{TestEnv, get, insert_tenant, insert_user, person, send, super_admin};

const X: &str = "/api/platform/export";
const CCA: &str = "job/customer_care_agent";

fn code(b: &Value) -> &str {
    b["error"]["code"].as_str().unwrap_or("")
}

async fn export(e: &TestEnv, query: &str) -> (StatusCode, Value) {
    send(&e.app, get(&format!("{X}{query}"), Some(&super_admin()))).await
}

async fn count(db: &PgPool, sql: &'static str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(db).await.unwrap()
}

#[tokio::test]
async fn export_needs_super_admin() {
    let e = crate::common::env().await;
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, "Tenant a").await;
    insert_user(&e.owner, "u-ta", t, "tenant_admin").await;
    let (s, b) = send(&e.app, get(X, Some(&person("u-ta", &[])))).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
    assert_eq!(code(&b), "super_admin_required");
}

#[tokio::test]
async fn export_family_filter() {
    let e = crate::common::env().await;
    let (s, doc) = export(&e, "?family=job.customer_care_agent").await;
    assert_eq!(s, StatusCode::OK, "{doc}");
    assert_eq!(doc["family"], CCA);
    let layers = doc["layers"].as_array().unwrap();
    assert!(!layers.is_empty());
    assert!(layers.iter().all(|l| l["family"] == CCA), "{layers:?}");
    let tags = doc["tags"].as_array().unwrap();
    assert!(!tags.is_empty() && tags.iter().all(|t| t["family"] == CCA));
    let rows = doc["rows"].as_array().unwrap();
    assert!(
        rows.iter()
            .all(|r| layers.iter().any(|l| l["key"] == r["layer_key"]))
    );

    let v: i32 = sqlx::query_scalar(
        "SELECT max(version) FROM layer_tag WHERE family = $1 AND NOT provisional",
    )
    .bind(CCA)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    let (s, one) = export(&e, &format!("?family={CCA}&tag=v{v}")).await;
    assert_eq!(s, StatusCode::OK, "{one}");
    let members: i64 =
        sqlx::query_scalar("SELECT count(*) FROM layer_tag_row WHERE family = $1 AND version = $2")
            .bind(CCA)
            .bind(v)
            .fetch_one(&e.owner)
            .await
            .unwrap();
    assert_eq!(one["rows"].as_array().unwrap().len() as i64, members);
    assert_eq!(one["tags"].as_array().unwrap().len(), 1);
    assert_eq!(one["tags"][0]["version"], v);

    for (q, status, want) in [
        (
            "?family=job.b6b_nobody",
            StatusCode::NOT_FOUND,
            "layer_not_found",
        ),
        (
            "?family=job.customer_care_agent&tag=v999",
            StatusCode::NOT_FOUND,
            "tag_not_found",
        ),
        ("?tag=v1", StatusCode::BAD_REQUEST, "invalid_query"),
    ] {
        let (s, b) = export(&e, q).await;
        assert_eq!(s, status, "{q}: {b}");
        assert_eq!(code(&b), want, "{q}");
    }
}

#[tokio::test]
async fn export_never_writes_rows() {
    let e = crate::common::env().await;
    let before = (
        count(&e.owner, "SELECT count(*) FROM layer_row").await,
        count(&e.owner, "SELECT count(*) FROM layer_tag").await,
        count(&e.owner, "SELECT count(*) FROM layer").await,
    );
    let audits = count(&e.owner, "SELECT count(*) FROM platform_audit").await;
    let (s, b) = export(&e, "").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let after = (
        count(&e.owner, "SELECT count(*) FROM layer_row").await,
        count(&e.owner, "SELECT count(*) FROM layer_tag").await,
        count(&e.owner, "SELECT count(*) FROM layer").await,
    );
    assert_eq!(before, after);
    assert_eq!(
        count(&e.owner, "SELECT count(*) FROM platform_audit").await,
        audits + 1
    );
    assert_eq!(
        count(
            &e.owner,
            "SELECT count(*) FROM platform_audit WHERE action = 'platform.export'"
        )
        .await,
        1
    );
}
