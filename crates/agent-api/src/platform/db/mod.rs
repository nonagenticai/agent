//! The platform layer store (plan B6 §1–§4 over migrations 0041–0043). Every function takes the caller's
//! connection, which must be `agent_platform` for writes (crate::platform_db `begin_platform`); the readers also
//! work as `agent_app` (SELECT is granted to both). Nothing here opens or commits a transaction.
//!
//! - [`authored`]: B6b authored writes: `write_authored` (duplicate_add / add_tombstoned / stale_revision) and
//!   `insert_layer`.
//! - [`layers`]: layers, families, per-layer summaries.
//! - [`profiles`]: published `profile` rows for selection options and pinned rows for a release's `profile_values`.
//! - [`rows`]: revision writes (`write_revision`), head rows, revisions, override targets, untagged drafts.
//! - [`tags`]: family tags (`tag_family`), members, the stack, `latest_tags`, `tag_content_hash`.
//! - [`vocab`]: the insert-only vocabularies (axes, axis rules, dimensions).
//!
//! Writes of one family are serialised by [`lock_family`], taken by every writer before it reads.
pub mod authored;
pub mod layers;
pub mod profiles;
pub mod rows;
pub mod tags;
pub mod vocab;

use axum::http::StatusCode;
use serde_json::Value;
use sqlx::PgConnection;

use crate::error::{ApiError, ApiResult};

pub use layers::{Family, Layer, LayerSummary};
pub use rows::{DraftRow, KeyedRow, OverrideTarget, Row};
pub use tags::{StackEntry, TagMember, TemplateTag};

/// The first key of the two-key advisory lock a family write holds (`pg_advisory_xact_lock(space,
/// hashtext(family))`). Two-key locks never collide with the one-key locks the rest of the crate takes.
pub const FAMILY_LOCK_SPACE: i32 = 0x0B6A_0007;

/// Serialise every write of `family` (revision writes and tags) until the transaction ends.
pub async fn lock_family(conn: &mut PgConnection, family: &str) -> ApiResult<()> {
    sqlx::query("SELECT pg_advisory_xact_lock($1, hashtext($2))")
        .bind(FAMILY_LOCK_SPACE)
        .bind(family)
        .execute(conn)
        .await?;
    Ok(())
}

/// A 422 whose error object carries `errors` (a list of agent-model `FieldError`s, or any JSON list).
pub(crate) fn unprocessable(
    code: &'static str,
    message: impl Into<String>,
    errors: Value,
) -> ApiError {
    ApiError::detailed(StatusCode::UNPROCESSABLE_ENTITY, code, message).with("errors", errors)
}

/// A bug in stored platform data (a scope or kind the schema should have rejected): a 500.
pub(crate) fn corrupt(what: String) -> ApiError {
    ApiError::Internal(anyhow::anyhow!("platform store: {what}"))
}
