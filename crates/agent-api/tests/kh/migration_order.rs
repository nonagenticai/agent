//! KH-F1: production ran main's migrations (B5's 0140–0148 and B7's 0150 included) before KH's 0132–0139 ship, so it
//! applies KH's AFTER them, while a fresh database applies everything in number order. KH 0133 / 0136 / 0138 and B5
//! 0141 all rewrite `release_conflict_object_check`; each writes the full object union, so both orders leave the same
//! `release_conflict` constraints (sqlx 0.9 `run_direct` has no out-of-order refusal, `tests/migration_order.rs`).
//! KH-F2: 0133 also rewrites `layer_row_kind_check` and `job_overlay_row_object_check`; the only later rewrite is
//! B8's 0191 and rule-slot 0222 expand layer kinds; 0229 adds tenant rule definitions to overlay objects. Both orders
//! leave the same `layer_row` / `job_overlay_row` checks.
use std::borrow::Cow;

use sqlx::PgPool;

use crate::common;

/// KH's migrations.
fn is_kh(v: i64) -> bool {
    (132..=139).contains(&v)
}

/// The full object union every rewrite of `release_conflict_object_check` writes.
const OBJECTS: [&str; 14] = [
    "activity",
    "context_space",
    "ontology_attribute",
    "gate_option",
    "knowledge_store",
    "job_setting",
    "knowledge_pack",
    "instruction",
    "knowledge_kind",
    "entity",
    "gate",
    "gate_question",
    "narrowing",
    "procedure",
];

async fn fresh() -> PgPool {
    let url = common::fresh_database().await;
    PgPool::connect(&url).await.expect("owner pool")
}

/// `table`'s CHECK constraints as `name: definition`, sorted.
async fn checks_of(owner: &PgPool, table: &str) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT conname || ': ' || pg_get_constraintdef(oid) FROM pg_constraint \
          WHERE conrelid = $1::regclass AND contype = 'c' ORDER BY conname",
    )
    .bind(table)
    .fetch_all(owner)
    .await
    .unwrap_or_else(|e| panic!("read {table} constraints: {e}"))
}

/// `release_conflict`'s CHECK constraints as `name: definition`, sorted.
async fn conflict_checks(owner: &PgPool) -> Vec<String> {
    checks_of(owner, "release_conflict").await
}

/// The migration files (by name) whose SQL names `constraint`.
fn migrations_naming(constraint: &str) -> Vec<String> {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/migrations");
    let mut out: Vec<String> = std::fs::read_dir(dir)
        .expect("read the migrations directory")
        .map(|f| f.expect("a migration entry").path())
        .filter(|p| p.extension().is_some_and(|x| x == "sql"))
        .filter(|p| {
            std::fs::read_to_string(p)
                .expect("read a migration")
                .contains(constraint)
        })
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    out.sort();
    out
}

/// KH-F2: 0229 adds rule_slot to the full overlay object union, and 0222 is the last migration that
/// rewrites `layer_row_kind_check` (0191's full list plus `rule_slot`; 0191 added `journey_channel`, `stage_scope`, `use_case_card`). A later
/// rewrite must write the full list, and then belongs in the order test below.
#[test]
fn kind_and_overlay_constraint_rewrites_are_accounted_for() {
    assert_eq!(
        migrations_naming("layer_row_kind_check"),
        [
            "0041_platform_layers.sql",
            "0125_knowledge_pack_kind.sql",
            "0133_instruction_kind.sql",
            "0191_journey_kinds.sql",
            "0222_rule_slot_kind.sql"
        ]
    );
    assert_eq!(
        migrations_naming("job_overlay_row_object_check"),
        [
            "0047_job_design.sql",
            "0125_knowledge_pack_kind.sql",
            "0133_instruction_kind.sql",
            "0229_tenant_rule_definitions.sql"
        ]
    );
}

#[tokio::test]
async fn kh_after_main_and_fresh_order_leave_the_same_conflict_checks() {
    // production: main (everything but KH, B5's 0141 included), then the release adds KH's migrations
    let prod = fresh().await;
    let mut partial = sqlx::migrate!("./migrations");
    partial.migrations = Cow::Owned(
        partial
            .iter()
            // 0180+ were written after KH reached every database (D116 0180 names 0139's `region`)
            .filter(|m| !is_kh(m.version) && m.version < 180)
            .cloned()
            .collect(),
    );
    assert!(
        partial.iter().any(|m| m.version == 141),
        "B5's 0141 is in main's run"
    );
    partial.run(&prod).await.expect("migrate main without KH");
    sqlx::migrate!("./migrations")
        .run(&prod)
        .await
        .expect("then KH's migrations after main's");

    // a fresh database: number order (KH's 0138 before B5's 0141)
    let numbered = fresh().await;
    sqlx::migrate!("./migrations")
        .run(&numbered)
        .await
        .expect("migrate in number order");

    let checks = conflict_checks(&numbered).await;
    assert_eq!(
        conflict_checks(&prod).await,
        checks,
        "release_conflict's checks are the same in both orders"
    );
    for table in ["layer_row", "job_overlay_row", "business_rule"] {
        assert_eq!(
            checks_of(&prod, table).await,
            checks_of(&numbered, table).await,
            "{table}'s checks are the same in both orders"
        );
    }
    let kinds = checks_of(&numbered, "layer_row").await;
    let kinds = kinds
        .iter()
        .find(|c| c.starts_with("layer_row_kind_check: "))
        .expect("the kind check");
    assert!(kinds.contains("'instruction'"), "{kinds}");
    let overlay = checks_of(&numbered, "job_overlay_row").await;
    let overlay = overlay
        .iter()
        .find(|c| c.starts_with("job_overlay_row_object_check: "))
        .expect("the overlay object check");
    for o in [
        "gate_option",
        "ontology_attribute",
        "knowledge_pack",
        "instruction",
        "rule_slot",
    ] {
        assert!(
            overlay.contains(&format!("'{o}'")),
            "{o} missing: {overlay}"
        );
    }
    let objects = checks
        .iter()
        .find(|c| c.starts_with("release_conflict_object_check: "))
        .expect("the object check");
    for o in OBJECTS {
        assert!(
            objects.contains(&format!("'{o}'")),
            "{o} missing: {objects}"
        );
    }
}
