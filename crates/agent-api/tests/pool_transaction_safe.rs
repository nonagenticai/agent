//! state::transaction_safe: a connection released while the server still has a transaction open on it is closed,
//! so the next `begin()` never joins a stranger's transaction.

use crate::common;

use sqlx::postgres::PgPoolOptions;

#[tokio::test]
async fn a_connection_left_in_a_transaction_is_never_reused() {
    let url = common::fresh_database().await;
    let pool = agent_api::state::transaction_safe(PgPoolOptions::new())
        .max_connections(1)
        .connect(&url)
        .await
        .unwrap();
    // what a cancelled `begin()` leaves behind: BEGIN on the server, nothing tracked by sqlx
    {
        let mut c = pool.acquire().await.unwrap();
        sqlx::raw_sql("BEGIN").execute(&mut *c).await.unwrap();
        sqlx::query("SELECT 1").execute(&mut *c).await.unwrap();
    }
    // the only connection was closed on release: a new one opens, and a transaction starts cleanly on it
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    // an ordinary release keeps the connection
    let pid1: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&pool)
        .await
        .unwrap();
    let pid2: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(pid1, pid2, "a clean connection is reused");
}
