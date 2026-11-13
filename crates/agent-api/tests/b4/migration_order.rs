//! B4 migration order (§0 R2). Production already ran every migration on main (≤0122, plus B7's 0097, B13's
//! 0110–0115 and 0124, and PK's 0125–0127) when B4 ships 0062, 0064, 0065, 0066, 0068, 0123 and 0128, so production
//! applies B4's migrations AFTER main's, while a fresh database applies everything in number order (0065 before 0067
//! and 0091–0127; 0123 before 0124–0127; 0128 last). sqlx 0.9 `run_direct` has no out-of-order refusal (see
//! `tests/migration_order.rs`). Both orders must work and leave `agent_runtime` with the same grants.
use std::borrow::Cow;

use sqlx::PgPool;

use crate::b4::runtime_role::runtime_grants;
use crate::common;

/// B4's migrations below 0123 (those not on this branch yet are simply absent from the source). 0069 (T16, the
/// `handover_policy` working row) ships after production ran ≤0130, so it too is applied after every later number.
const B4_LOW: [i64; 6] = [62, 64, 65, 66, 68, 69];
/// B4's migrations above 0122 (0124 is B13's web chat sessions, 0125–0127 PK's; add later B4 migrations here).
/// 0137 (KH T7) widens 0123's `provision_outbox` column grant, so it belongs with the runtime grants applied after
/// main's.
/// 0183 (known-customer web chat) adds web chat session / conversation columns, the assertion-jti table and the
/// `step_up_required` handover reason; it references only objects ≤0179.
/// 0184 (known-customer display name) adds one column to `web_chat_session` (0124 + 0183).
/// 0200 (SW T1) grants `agent_runtime` the owner writes (`conversation_control`, item `closed_by`), so 0065's REVOKE
/// must run before it.
/// 0205 (SW-9 attachments) grants `agent_runtime` SELECT on `conversation_attachment`, so 0065's REVOKE must run
/// before it. 0223 (SW-9, bytes move to artifact_service) alters 0205's table and adds the tombstone / upload-intent
/// outboxes, so it travels with 0205.
/// 0196 (known-customer hardening) replaces 0182's `party_mark_verified` grant to `agent_app` with
/// `party_mark_session_verified`, so it runs after 0182 and 0183.
/// 0221 (staff test chats) widens 0183's `conversation.identity_source` check, so it runs after 0183.
/// 0236 (A1, identity into conversation state) backfills from and drops the columns 0182 / 0183 / 0184 / 0221 added
/// and the functions 0182 / 0196 created, so it runs after them.
/// 0233 (A5) rewrites `usage_event`'s source check and its `customer_turn` / `customers:` rows, and `usage_event` is
/// 0064's (B4), so 0233 runs after it.
const B4_HIGH: [i64; 18] = [
    123, 128, 129, 131, 137, 154, 179, 181, 182, 183, 184, 196, 200, 205, 221, 223, 233, 236,
];

fn is_b4(v: i64) -> bool {
    B4_LOW.contains(&v) || B4_HIGH.contains(&v)
}

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
async fn b4_applied_after_0122_and_fresh_order() {
    // production: everything but B4 (main), then the release adds B4's migrations
    let prod = fresh().await;
    let mut partial = sqlx::migrate!("./migrations");
    partial.migrations = Cow::Owned(
        partial
            .iter()
            .filter(|m| !is_b4(m.version))
            .cloned()
            .collect(),
    );
    for v in [115, 122, 124, 125, 126, 127] {
        assert!(
            partial.iter().any(|m| m.version == v),
            "main's {v} is in the first run"
        );
    }
    partial.run(&prod).await.expect("migrate main without B4");
    sqlx::migrate!("./migrations")
        .run(&prod)
        .await
        .expect("then B4's migrations after main's");
    assert_eq!(
        applied_versions(&prod).await,
        all_versions(),
        "every version applied exactly once"
    );
    let b4: Vec<i64> = all_versions().into_iter().filter(|v| is_b4(*v)).collect();
    for v in [62, 64, 65, 66, 69, 123, 128, 129, 131, 137, 154] {
        assert!(b4.contains(&v), "B4 migration {v} is in the source");
    }
    let after: bool = sqlx::query_scalar(
        "SELECT (SELECT min(installed_on) FROM _sqlx_migrations WHERE version = ANY ($1))
              > (SELECT max(installed_on) FROM _sqlx_migrations WHERE NOT version = ANY ($1))",
    )
    .bind(&b4)
    .fetch_one(&prod)
    .await
    .expect("compare installed_on");
    assert!(after, "B4's migrations were applied after main's");

    // a fresh database: number order
    let numbered = fresh().await;
    sqlx::migrate!("./migrations")
        .run(&numbered)
        .await
        .expect("migrate in number order");
    assert_eq!(applied_versions(&numbered).await, all_versions());

    assert_eq!(
        runtime_grants(&prod).await,
        runtime_grants(&numbered).await,
        "agent_runtime holds the same grants in both orders"
    );
    // 0069 references only objects ≤0069: the working row has the same shape and agent_app grants in both orders
    assert_eq!(
        handover_policy_shape(&prod).await,
        handover_policy_shape(&numbered).await,
        "handover_policy is the same in both orders"
    );
    assert!(
        !handover_policy_shape(&numbered).await.is_empty(),
        "0069 created handover_policy"
    );
    // 0183 references only objects ≤0179: the jti table, the new columns and the widened reason CHECK are the same in
    // both orders
    for table in ["webchat_assertion_jti", "web_chat_session", "conversation"] {
        assert_eq!(
            table_shape(&prod, table).await,
            table_shape(&numbered, table).await,
            "{table} is the same in both orders"
        );
    }
    assert!(
        table_shape(&numbered, "webchat_assertion_jti")
            .await
            .iter()
            .any(|r| r.starts_with("col jti ")),
        "0183 created webchat_assertion_jti"
    );
    // 0066 references only objects ≤0066 (T16a): the same tenant_calendar in both orders
    assert_eq!(
        table_shape(&prod, "tenant_calendar").await,
        table_shape(&numbered, "tenant_calendar").await,
        "tenant_calendar is the same in both orders"
    );
    assert!(
        !table_shape(&numbered, "tenant_calendar").await.is_empty(),
        "0066 created tenant_calendar"
    );
}

/// `table`'s columns, constraints, policies and agent_app / agent_runtime privileges (column grants included), as
/// sorted text rows.
async fn table_shape(owner: &PgPool, table: &str) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT 'col ' || column_name || ' ' || data_type || ' ' || is_nullable FROM information_schema.columns \
          WHERE table_schema = 'public' AND table_name = $1 \
         UNION ALL SELECT 'con ' || conname || ' ' || pg_get_constraintdef(oid) FROM pg_constraint \
          WHERE conrelid = to_regclass('public.' || $1) \
         UNION ALL SELECT 'pol ' || policyname || ' ' || permissive FROM pg_policies \
          WHERE schemaname = 'public' AND tablename = $1 \
         UNION ALL SELECT 'priv ' || grantee || ' ' || privilege_type FROM information_schema.role_table_grants \
          WHERE table_schema = 'public' AND table_name = $1 AND grantee IN ('agent_app', 'agent_runtime') \
         UNION ALL SELECT 'colpriv ' || grantee || ' ' || column_name || ' ' || privilege_type \
           FROM information_schema.column_privileges \
          WHERE table_schema = 'public' AND table_name = $1 AND grantee = 'agent_runtime' \
         ORDER BY 1",
    )
    .bind(table.to_owned())
    .fetch_all(owner)
    .await
    .expect("read the table shape")
}

/// `handover_policy`'s columns, constraints, policies and agent_app privileges, as sorted text rows.
async fn handover_policy_shape(owner: &PgPool) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT 'col ' || column_name || ' ' || data_type || ' ' || is_nullable FROM information_schema.columns \
          WHERE table_schema = 'public' AND table_name = 'handover_policy' \
         UNION ALL SELECT 'con ' || conname || ' ' || pg_get_constraintdef(oid) FROM pg_constraint \
          WHERE conrelid = to_regclass('public.handover_policy') \
         UNION ALL SELECT 'pol ' || policyname || ' ' || permissive FROM pg_policies \
          WHERE schemaname = 'public' AND tablename = 'handover_policy' \
         UNION ALL SELECT 'priv ' || privilege_type FROM information_schema.role_table_grants \
          WHERE table_schema = 'public' AND table_name = 'handover_policy' AND grantee = 'agent_app' \
         ORDER BY 1",
    )
    .fetch_all(owner)
    .await
    .expect("read handover_policy shape")
}
