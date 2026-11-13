//! The `agent_platform` role (B1 X4; spec 01 §5.2 G4). Platform tables (no tenant column, no RLS: the signup
//! queue and journal today; B6–B8 add theirs) grant nothing to `agent_app`; they are reached only as
//! `agent_platform`, which `agent_app` may `SET ROLE` to (membership `INHERIT FALSE, SET TRUE`, migration 0031)
//! but inherits nothing from.
//!
//! - [`begin_platform`]: a transaction that is `agent_platform` throughout.
//! - [`enter`] / [`leave`]: switch inside an open transaction. A tenant GUC set by `begin_scoped` survives the
//!   switch (it is a transaction-local setting, not a role property), so a mixed transaction reads
//!   `begin_scoped(t)` → tenant writes → `enter` → platform writes → `leave` → COMMIT.
//!
//! `SET LOCAL ROLE` ends with the transaction, so a pooled connection always returns as `agent_app`. Never call
//! these inside a SECURITY DEFINER function.
use sqlx::{PgConnection, PgPool, Postgres, Transaction};

use crate::error::ApiResult;

pub const PLATFORM_ROLE: &str = "agent_platform";

/// `BEGIN; SET LOCAL ROLE agent_platform`.
pub async fn begin_platform(db: &PgPool) -> ApiResult<Transaction<'static, Postgres>> {
    let mut tx = db.begin().await?;
    enter(&mut tx).await?;
    Ok(tx)
}

/// `SET LOCAL ROLE agent_platform` for the rest of the open transaction (or until [`leave`]).
pub async fn enter(conn: &mut PgConnection) -> ApiResult<()> {
    sqlx::query("SET LOCAL ROLE agent_platform")
        .execute(conn)
        .await?;
    Ok(())
}

/// Back to the session role (`agent_app`) for the rest of the transaction.
pub async fn leave(conn: &mut PgConnection) -> ApiResult<()> {
    sqlx::query("RESET ROLE").execute(conn).await?;
    Ok(())
}
