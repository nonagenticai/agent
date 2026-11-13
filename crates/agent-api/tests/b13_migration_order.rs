//! B13 migration order. Production has applied everything up to 0122 when B13 ships 0110, 0112–0115 and 0124,
//! so production applies 0110–0114 AFTER 0120–0122 while a fresh database applies everything in number order (0110–
//! 0114 BEFORE 0120–0122). Both orders must work: each B13 migration references only objects of a lower number (and
//! never 0120–0122 from 0110–0114). See `b12_migration_order.rs` for the sqlx behaviour this relies on.
use crate::common;

use std::borrow::Cow;

use sqlx::PgPool;

/// B13's migrations.
const B13: [i64; 6] = [110, 112, 113, 114, 115, 124];
/// The highest version production had applied before B13.
const PROD_HEAD: i64 = 122;

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

#[test]
fn every_b13_migration_exists() {
    let all = all_versions();
    for v in B13 {
        assert!(all.contains(&v), "B13 migration {v} is in ./migrations");
    }
}

/// Production: everything up to 0122 without B13, then the release adds B13.
#[tokio::test]
async fn b13_applies_on_top_of_0122() {
    let owner = fresh().await;
    let mut before = sqlx::migrate!("./migrations");
    before.migrations = Cow::Owned(
        before
            .iter()
            .filter(|m| m.version <= PROD_HEAD && !B13.contains(&m.version))
            .cloned()
            .collect(),
    );
    before
        .run(&owner)
        .await
        .expect("migrate to 0122 without B13");
    sqlx::migrate!("./migrations")
        .run(&owner)
        .await
        .expect("then the full set (B13 after 0120..0122)");
    assert_eq!(
        applied_versions(&owner).await,
        all_versions(),
        "every version applied exactly once"
    );
    let b13_after: bool = sqlx::query_scalar(
        "SELECT (SELECT min(installed_on) FROM _sqlx_migrations WHERE version = ANY ($1)) \
              > (SELECT max(installed_on) FROM _sqlx_migrations WHERE version BETWEEN 120 AND $2)",
    )
    .bind(B13.to_vec())
    .bind(PROD_HEAD)
    .fetch_one(&owner)
    .await
    .expect("compare installed_on");
    assert!(b13_after, "B13 was applied after 0120..0122");
}

/// A fresh database: number order, so 0110..0114 run before 0120..0122.
#[tokio::test]
async fn b13_number_order_fresh() {
    let owner = fresh().await;
    sqlx::migrate!("./migrations")
        .run(&owner)
        .await
        .expect("migrate in number order");
    assert_eq!(applied_versions(&owner).await, all_versions());
}
