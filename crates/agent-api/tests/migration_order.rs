//! Migration order. Production already ran main's migrations (≤0043) when B7 shipped 0091–0093; B2's 0044–0047
//! land later, so production applies 0091–0093 BEFORE 0044–0047 while a fresh database applies them in number
//! order. Both orders must work.
//!
//! sqlx 0.9.0 (`sqlx-core/src/migrate/migrator.rs`, `Migrator::run_direct`) applies every migration of the source
//! that `_sqlx_migrations` lacks, in version order, whatever the highest applied version is: there is no
//! "out of order" refusal. `validate_applied_migrations` only refuses an APPLIED version that is missing from the
//! source (`VersionMissing`, unless `ignore_missing`), and an applied migration whose checksum changed
//! (`VersionMismatch`). `state::build` runs `sqlx::migrate!("./migrations").run(&owner)` with the defaults
//! (`ignore_missing: false`, `locking: true`).
//!
//! The cut: 0091–0093 reference only `tenants`, `app_current_tenant()` (0001) and the `agent_app` /
//! `agent_platform` roles, nothing from 0041–0047, so "everything below 0041 plus everything from 0091" is a
//! valid first state, with 0041–0043 standing in for B2's 0044–0047 (which fall in the same range once merged).
use crate::common;

use std::borrow::Cow;
use std::ops::RangeInclusive;

use sqlx::PgPool;

/// Versions that production applies AFTER the connector migrations.
const LOWER_PENDING: RangeInclusive<i64> = 41..=90;
/// The first connector migration (B7).
const HIGHER_FIRST: i64 = 91;
/// The last connector migration (B7; 0094 adds routes + overrides, referencing only 0033 `jobs` and 0093). B12's 0120–0121 reference B12's 0061, so they ride with the lower pending set
/// (`tests/b12_migration_order.rs` covers their own order).
const HIGHER_LAST: i64 = 94;

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

#[tokio::test]
async fn higher_applied_first_then_lower_pending() {
    let url = common::fresh_database().await;
    let owner = PgPool::connect(&url).await.expect("owner pool");

    let mut partial = sqlx::migrate!("./migrations");
    partial.migrations = Cow::Owned(
        partial
            .iter()
            .filter(|m| !LOWER_PENDING.contains(&m.version) && m.version <= HIGHER_LAST)
            .cloned()
            .collect(),
    );
    assert!(
        partial.iter().any(|m| m.version >= HIGHER_FIRST),
        "the connector migrations are in the first run"
    );
    partial
        .run(&owner)
        .await
        .expect("migrate without 0041..0090");

    sqlx::migrate!("./migrations")
        .run(&owner)
        .await
        .expect("full migrator applies the lower pending versions");

    let applied = applied_versions(&owner).await;
    assert_eq!(
        applied,
        all_versions(),
        "every version applied exactly once"
    );
    let pending_after: bool = sqlx::query_scalar(
        "SELECT (SELECT min(installed_on) FROM _sqlx_migrations WHERE version BETWEEN $1 AND $2)
              > (SELECT max(installed_on) FROM _sqlx_migrations WHERE version BETWEEN $3 AND $4)",
    )
    .bind(LOWER_PENDING.start())
    .bind(LOWER_PENDING.end())
    .bind(HIGHER_FIRST)
    .bind(HIGHER_LAST)
    .fetch_one(&owner)
    .await
    .expect("compare installed_on");
    assert!(pending_after, "0041.. were applied after 0091..");
}

#[tokio::test]
async fn number_order_fresh() {
    let url = common::fresh_database().await;
    let owner = PgPool::connect(&url).await.expect("owner pool");
    sqlx::migrate!("./migrations")
        .run(&owner)
        .await
        .expect("migrate in number order");
    assert_eq!(applied_versions(&owner).await, all_versions());
}
