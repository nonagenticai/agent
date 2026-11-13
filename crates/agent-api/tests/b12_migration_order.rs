//! B12 migration order. Production already ran main's migrations (≤0043, plus B7's 0091–0093) when B12 ships
//! 0060–0067 and 0120–0121; B2's 0044–0047 and B3's 0050–0057 land later, so production applies B12's migrations
//! BEFORE them while a fresh database applies everything in number order. Both orders must work.
//!
//! sqlx 0.9.0 (`Migrator::run_direct`) applies every migration of the source that `_sqlx_migrations` lacks, in
//! version order, whatever the highest applied version is (no "out of order" refusal); see `migration_order.rs` on
//! agent#18 for the walk through the sqlx source.
//!
//! The cut: 0060–0067 and 0120–0121 reference only `tenants`, `jobs`, `app_current_tenant()`, `app_current_job()`
//! (≤0033), each other, and the `agent_app` role — nothing from 0041–0059. So "everything up to 0040 plus B12"
//! is a valid first state, with 0041–0043 standing in for the B2/B3 migrations that fall in the same range.
use crate::common;

use std::borrow::Cow;
use std::ops::RangeInclusive;

use sqlx::PgPool;

/// Versions that production applies AFTER B12's migrations.
const LOWER_PENDING: RangeInclusive<i64> = 41..=59;
/// Versions written after B12 shipped (B4's 0062/0064–0066/0068; B4's 0123/0128 fall under `> 122`). They did not exist
/// when production ran B12's migrations, and they grant on / reference 0041–0059 tables, so the B12-era first run
/// excludes them; the full run applies them last, as production does.
const AFTER_B12: [i64; 5] = [62, 64, 65, 66, 68];
/// B12's migrations.
const B12: [i64; 6] = [60, 61, 63, 67, 120, 121];
/// Versions that shipped after B12 and are not part of its first state: B13's 0110, 0112–0115 and 0124
/// (`tests/b13_migration_order.rs` covers their order) and PK's 0125–0127, which reference 0041–0054 (layer_row,
/// job_release, job_overlay_row, knowledge_upload, source, platform_pack) and so ride with the lower pending set
/// (`tests/pk/schema.rs` covers their production order).
const LATER: [i64; 9] = [110, 112, 113, 114, 115, 124, 125, 126, 127];

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

#[tokio::test]
async fn b12_applied_before_the_lower_pending_versions() {
    let owner = fresh().await;
    let mut partial = sqlx::migrate!("./migrations");
    partial.migrations = Cow::Owned(
        partial
            .iter()
            .filter(|m| {
                !LOWER_PENDING.contains(&m.version)
                    && !AFTER_B12.contains(&m.version)
                    && !LATER.contains(&m.version)
                    && m.version <= 122
            })
            .cloned()
            .collect(),
    );
    for v in B12 {
        assert!(
            partial.iter().any(|m| m.version == v),
            "B12 migration {v} is in the first run"
        );
    }
    partial
        .run(&owner)
        .await
        .expect("migrate without 0041..0059");

    sqlx::migrate!("./migrations")
        .run(&owner)
        .await
        .expect("full migrator applies the lower pending versions");

    assert_eq!(
        applied_versions(&owner).await,
        all_versions(),
        "every version applied exactly once"
    );
    let pending_after: bool = sqlx::query_scalar(
        "SELECT (SELECT min(installed_on) FROM _sqlx_migrations WHERE version BETWEEN $1 AND $2)
              > (SELECT max(installed_on) FROM _sqlx_migrations WHERE version = ANY ($3))",
    )
    .bind(LOWER_PENDING.start())
    .bind(LOWER_PENDING.end())
    .bind(B12.to_vec())
    .fetch_one(&owner)
    .await
    .expect("compare installed_on");
    assert!(pending_after, "0041.. were applied after B12");
}

/// Production today: main's migrations (≤0043) are applied; the release adds B12 on top.
#[tokio::test]
async fn b12_applies_on_top_of_0043() {
    let owner = fresh().await;
    sqlx::migrate!("./migrations")
        .run_to(43, &owner)
        .await
        .expect("migrate to 0043");
    sqlx::migrate!("./migrations")
        .run(&owner)
        .await
        .expect("then the full set");
    assert_eq!(applied_versions(&owner).await, all_versions());
}

#[tokio::test]
async fn number_order_fresh() {
    let owner = fresh().await;
    sqlx::migrate!("./migrations")
        .run(&owner)
        .await
        .expect("migrate in number order");
    assert_eq!(applied_versions(&owner).await, all_versions());
}
