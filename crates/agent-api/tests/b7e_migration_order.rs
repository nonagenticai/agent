//! B7 remaining (workstream E) migration order. Production has applied everything up to 0124 when 0098
//! (`agent_case.external_ref`) ships, so production applies 0098 AFTER 0110–0124 while a fresh database applies it in
//! number order. Both orders must work: 0098 references only 0060 (`agent_case`). See `b12_migration_order.rs` for
//! the sqlx behaviour this relies on.
use crate::common;

use std::borrow::Cow;

use sqlx::PgPool;

/// Workstream E's migration.
const B7E: i64 = 98;
/// The highest version production had applied before 0098.
const PROD_HEAD: i64 = 124;

async fn applied_versions(owner: &PgPool) -> Vec<i64> {
    sqlx::query_scalar("SELECT version FROM _sqlx_migrations WHERE success ORDER BY version")
        .fetch_all(owner)
        .await
        .expect("read _sqlx_migrations")
}

fn all_versions() -> Vec<i64> {
    let mut v: Vec<i64> = sqlx::migrate!("./migrations")
        .iter()
        .map(|m| m.version)
        .collect();
    v.sort_unstable();
    v
}

async fn fresh() -> PgPool {
    let url = common::fresh_database().await;
    PgPool::connect(&url).await.expect("owner pool")
}

async fn has_external_ref(owner: &PgPool) -> bool {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.columns \
          WHERE table_name = 'agent_case' AND column_name = 'external_ref' AND is_nullable = 'YES')",
    )
    .fetch_one(owner)
    .await
    .expect("read information_schema")
}

#[test]
fn migration_0098_exists() {
    assert!(all_versions().contains(&B7E), "0098 is in ./migrations");
}

/// Production: everything up to 0124 without 0098, then the release adds 0098.
#[tokio::test]
async fn b7e_applies_on_top_of_0124() {
    let owner = fresh().await;
    let mut before = sqlx::migrate!("./migrations");
    before.migrations = Cow::Owned(
        before
            .iter()
            .filter(|m| m.version <= PROD_HEAD && m.version != B7E)
            .cloned()
            .collect(),
    );
    before
        .run(&owner)
        .await
        .expect("migrate to 0124 without 0098");
    assert!(
        !has_external_ref(&owner).await,
        "no external_ref before 0098"
    );
    sqlx::migrate!("./migrations")
        .run(&owner)
        .await
        .expect("then the full set (0098 after 0124)");
    assert_eq!(
        applied_versions(&owner).await,
        all_versions(),
        "every version applied exactly once"
    );
    assert!(
        has_external_ref(&owner).await,
        "agent_case.external_ref exists"
    );
}

/// A fresh database: number order.
#[tokio::test]
async fn b7e_number_order_fresh() {
    let owner = fresh().await;
    sqlx::migrate!("./migrations")
        .run(&owner)
        .await
        .expect("migrate in number order");
    assert_eq!(applied_versions(&owner).await, all_versions());
    assert!(
        has_external_ref(&owner).await,
        "agent_case.external_ref exists"
    );
}
