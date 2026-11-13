//! B1 T3: the `agent_platform` role split (migration 0031, crate::platform_db, the boot check). Real Postgres 18
//! (one container for this binary).
//!
//! Roles are server-wide, and some tests here change the `agent_platform` role or its membership for a moment,
//! so every test in this binary holds [`LOCK`]: nothing else migrates a database on this server meanwhile.
mod common;

use agent_api::platform_db::{begin_platform, enter, leave};
use agent_api::tenancy::begin_scoped;
use common::insert_tenant;
use uuid::Uuid;

static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn sqlstate(e: &sqlx::Error) -> Option<String> {
    e.as_database_error()
        .and_then(|d| d.code())
        .map(|c| c.into_owned())
}

#[tokio::test]
async fn app_role_cannot_touch_signup_tables() {
    let _l = LOCK.lock().await;
    let e = common::env().await;
    let db = &e.state.db;
    for sql in [
        "SELECT count(*) FROM signup_approvals",
        "SELECT count(*) FROM signup_provisioning_steps",
        "INSERT INTO signup_approvals (email, name, source, intake_client) \
         VALUES ('a@b.co', 'A', 'web', 'agent-signup-intake')",
        "UPDATE signup_provisioning_steps SET attempts = 0",
        "SELECT count(*) FROM agent_list_tenants()",
    ] {
        let err = sqlx::query(sql).execute(db).await.unwrap_err();
        assert_eq!(sqlstate(&err).as_deref(), Some("42501"), "{sql}: {err}");
    }
    // control: the same reads as agent_platform are allowed
    let mut tx = begin_platform(db).await.unwrap();
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM signup_approvals")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(n, 0);
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM agent_list_tenants()")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(n, 0);
    tx.commit().await.unwrap();
    // and the pooled connection came back as agent_app
    let who: String = sqlx::query_scalar("SELECT current_user::text")
        .fetch_one(db)
        .await
        .unwrap();
    assert_eq!(who, "agent_app");
}

#[tokio::test]
async fn platform_role_can_and_gucs_survive_the_switch() {
    let _l = LOCK.lock().await;
    let e = common::env().await;
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, "Alpha").await;
    let mut tx = begin_scoped(&e.state.db, t).await.unwrap();
    enter(&mut tx).await.unwrap();
    let (who, guc): (String, String) =
        sqlx::query_as("SELECT current_user::text, current_setting('app.current_tenant')")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
    assert_eq!(who, "agent_platform");
    assert_eq!(guc, t.to_string(), "the tenant GUC survives SET ROLE");
    sqlx::query(
        "INSERT INTO signup_approvals (email, name, source, intake_client) \
         VALUES ('a@b.co', 'A', 'web', 'agent-signup-intake')",
    )
    .execute(&mut *tx)
    .await
    .expect("agent_platform writes the signup queue");
    leave(&mut tx).await.unwrap();
    let (who, guc): (String, String) =
        sqlx::query_as("SELECT current_user::text, current_setting('app.current_tenant')")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
    assert_eq!(who, "agent_app");
    assert_eq!(guc, t.to_string());
    // back under RLS, scoped to the tenant
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM tenants")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(n, 1);
    tx.commit().await.unwrap();
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM signup_approvals")
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(n, 1, "the platform write committed with the transaction");
}

#[tokio::test]
async fn boot_refuses_inherit_true_membership() {
    let _l = LOCK.lock().await;
    let e = common::env().await;
    // control: the harness membership passes
    agent_api::state::refuse_rls_bypass(&e.state.db)
        .await
        .expect("INHERIT FALSE, SET TRUE boots");
    let admin = common::server_admin().await;
    sqlx::query("GRANT agent_platform TO agent_app WITH INHERIT TRUE, SET TRUE")
        .execute(&admin)
        .await
        .unwrap();
    let r = agent_api::state::refuse_rls_bypass(&e.state.db).await;
    common::grant_platform_membership(&admin).await;
    let err = format!(
        "{:#}",
        r.expect_err("an inheriting membership must refuse boot")
    );
    assert!(err.contains("INHERIT TRUE"), "{err}");
    agent_api::state::refuse_rls_bypass(&e.state.db)
        .await
        .expect("restored");
}

#[tokio::test]
async fn boot_refuses_missing_membership() {
    let _l = LOCK.lock().await;
    let e = common::env().await;
    let admin = common::server_admin().await;
    sqlx::query("REVOKE agent_platform FROM agent_app")
        .execute(&admin)
        .await
        .unwrap();
    let r = agent_api::state::refuse_rls_bypass(&e.state.db).await;
    common::grant_platform_membership(&admin).await;
    let err = format!("{:#}", r.expect_err("no membership must refuse boot"));
    assert!(err.contains("not a member of agent_platform"), "{err}");
    agent_api::state::refuse_rls_bypass(&e.state.db)
        .await
        .expect("restored");
}

#[tokio::test]
async fn migration_0031_raises_without_the_role() {
    let _l = LOCK.lock().await;
    let admin = common::server_admin().await;
    sqlx::query("ALTER ROLE agent_platform RENAME TO agent_platform_parked")
        .execute(&admin)
        .await
        .unwrap();
    let r = common::try_env().await;
    sqlx::query("ALTER ROLE agent_platform_parked RENAME TO agent_platform")
        .execute(&admin)
        .await
        .unwrap();
    let err = match r {
        Ok(_) => panic!("migrating without agent_platform must fail"),
        Err(e) => format!("{e:#}"),
    };
    assert!(err.contains("role agent_platform does not exist"), "{err}");
    // control: with the role back, a fresh database migrates and boots
    common::try_env().await.expect("boots with the role");
}
