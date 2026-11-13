//! The B6b platform-model routes (plan B6 §5, Revision 2026-10-22). Every route is `SuperAdmin` (R1: 403
//! `super_admin_required`), takes no `X-Active-Tenant-Id`, and runs on the platform plane:
//!
//! - reads: [`read_tx`] (`REPEATABLE READ READ ONLY`, `agent_platform`);
//! - writes: [`write_tx`] (`agent_platform`), one revision through `db::authored::write_authored` (or a vocab
//!   insert), and exactly one platform audit row through [`audit`].
//!
//! Areas (one file each, each with its own `routes()`; merged here, mounted in crate::routes):
//! - [`layers`]: `/api/layers*`, `/api/layer-rows/*` (T9a; T12b authoring).
//! - [`business`]: `/api/axes*`, `/api/axis-rules`, `/api/profiles*`, `/api/dimensions*` (T10, T12e, T12g).
//! - [`objects`]: `/api/platform/objects*`, `/api/platform/operations*`, `/api/platform/error-states` (T11, T12c,
//!   T12f).
//! - [`gates`]: `/api/platform/gates*`, `/api/platform/agent-roles*` (T12, T12d).
//! - [`journeys`]: `/api/platform/journeys*` (B8 T4, T4a, T4b: the journey designer).
//! - [`industries`]: `/api/platform/industries*` (industries + departments: create, rename, retire, restore).
//!
//! Errors use the UI's codes (§5) via [`missing`], [`conflict`], [`unprocessable`], [`blocked`]. Bodies are
//! `ApiJson<T>` of `#[serde(deny_unknown_fields)]` structs (400 `invalid_body`); path/body strings are parsed with
//! [`parse_kind`], [`parse_op`], [`check_snake_key`], [`check_label`], [`layer_from_slug`].
use agent_model::kinds::{Kind, Op};
use axum::Router;
use serde_json::Value;
use sqlx::{PgConnection, Postgres, Transaction};

use crate::audit::{self, Actor, PlatformEvent};
use crate::error::{ApiError, ApiResult};
use crate::platform_db;
use crate::state::AppState;
use crate::tenancy::SuperAdmin;

pub use crate::platform::db::authored::{Expect, LayerMeta, check_label};

pub mod business;
pub mod compile;
pub mod export;
pub mod gates;
pub mod industries;
pub mod journeys;
pub mod layers;
pub mod objects;
pub mod templates;

/// Every B6b platform-model route.
pub fn routes() -> Router<AppState> {
    Router::new()
        .merge(layers::routes())
        .merge(compile::routes())
        .merge(business::routes())
        .merge(objects::routes())
        .merge(gates::routes())
        .merge(journeys::routes())
        .merge(industries::routes())
        .merge(templates::routes())
        .merge(export::routes())
}

/// The super_admin behind a write: the audit actor and the `author` / `added_by` string (the subject).
#[derive(Debug, Clone)]
pub struct Who {
    pub actor: Actor,
    pub by: String,
}

impl Who {
    pub fn of(s: &SuperAdmin) -> Who {
        Who {
            actor: Actor::person(&s.0),
            by: s.0.sub.clone(),
        }
    }
}

/// A write transaction as `agent_platform` (`begin_platform`). Commit it after the audit row.
pub async fn write_tx(st: &AppState) -> ApiResult<Transaction<'static, Postgres>> {
    platform_db::begin_platform(&st.db).await
}

/// A read transaction: `REPEATABLE READ READ ONLY`, as `agent_platform` (one snapshot for a multi-query read;
/// `compilePreview` must never write). Drop it (rollback) when done.
pub async fn read_tx(st: &AppState) -> ApiResult<Transaction<'static, Postgres>> {
    let mut tx = st.db.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *tx)
        .await?;
    platform_db::enter(&mut tx).await?;
    Ok(tx)
}

/// The one platform audit row of a write (the action must be listed in crate::audit::vocab `PLATFORM`).
pub async fn audit(conn: &mut PgConnection, who: &Who, e: PlatformEvent) -> ApiResult<i64> {
    audit::record_platform(conn, &who.actor, e).await
}

/// 404 `<thing>_not_found`.
pub fn missing(code: &'static str, msg: impl Into<String>) -> ApiError {
    ApiError::Missing(code, msg.into())
}

/// 409.
pub fn conflict(code: &'static str, msg: impl Into<String>) -> ApiError {
    ApiError::Conflict(code, msg.into())
}

/// 422.
pub fn unprocessable(code: &'static str, msg: impl Into<String>) -> ApiError {
    ApiError::Unprocessable(code, msg.into())
}

/// 409 with a list in the error object: `value_in_use{blockers}`, `gate_in_use{jobs}`,
/// `field_in_use{operations}`, `cardinality_in_use{blockers}`.
pub fn blocked(
    code: &'static str,
    msg: impl Into<String>,
    field: &str,
    list: impl Into<Value>,
) -> ApiError {
    ApiError::Conflict(code, msg.into()).with(field, list)
}

/// A row kind from a path or body (`layer_row.kind`); 422 `invalid_row`.
pub fn parse_kind(s: &str) -> ApiResult<Kind> {
    Kind::parse(s).ok_or_else(|| unprocessable("invalid_row", format!("“{s}” is not a row kind.")))
}

/// `add` / `patch` / `tombstone`; 422 `invalid_row`.
pub fn parse_op(s: &str) -> ApiResult<Op> {
    match s {
        "add" => Ok(Op::Add),
        "patch" => Ok(Op::Patch),
        "tombstone" => Ok(Op::Tombstone),
        _ => Err(unprocessable(
            "invalid_row",
            format!("“{s}” is not an op (add, patch or tombstone)."),
        )),
    }
}

/// A snake_case key (`[a-z][a-z0-9_]{0,63}`, the agent-model name grammar): 422 `code` otherwise
/// (`invalid_value_key`, `invalid_axis_key`, `invalid_dimension_key`, `invalid_gate_key`, ...).
pub fn check_snake_key(code: &'static str, key: &str) -> ApiResult<()> {
    let mut chars = key.chars();
    let ok = matches!(chars.next(), Some('a'..='z'))
        && key.len() <= 64
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if ok {
        Ok(())
    } else {
        Err(unprocessable(
            code,
            format!(
                "“{key}” is not a key: lowercase letters, digits and _, starting with a letter, at most 64."
            ),
        ))
    }
}

/// A layer key from its URL slug: `industry_dept.retail.customer_service` → `industry_dept/retail/customer_service`
/// (templateTagDetail / tagLayerRevisions). Dimension `=` passes through.
pub fn layer_from_slug(slug: &str) -> String {
    slug.replace('.', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_and_ops() {
        assert_eq!(parse_kind("gate_question").unwrap(), Kind::GateQuestion);
        assert_eq!(parse_kind("nope").unwrap_err().code(), "invalid_row");
        assert_eq!(parse_op("tombstone").unwrap(), Op::Tombstone);
        assert_eq!(parse_op("delete").unwrap_err().code(), "invalid_row");
    }

    #[test]
    fn snake_keys() {
        assert!(check_snake_key("invalid_value_key", "bundles_2").is_ok());
        for bad in ["", "2x", "Bundles", "a-b", &"a".repeat(65)] {
            assert_eq!(
                check_snake_key("invalid_value_key", bad)
                    .unwrap_err()
                    .code(),
                "invalid_value_key",
                "{bad}"
            );
        }
    }

    #[test]
    fn slugs() {
        assert_eq!(
            layer_from_slug("industry_dept.retail.customer_service"),
            "industry_dept/retail/customer_service"
        );
        assert_eq!(layer_from_slug("base"), "base");
    }

    #[test]
    fn blocked_carries_list() {
        let e = blocked("value_in_use", "in use", "blockers", serde_json::json!([]));
        assert_eq!(e.code(), "value_in_use");
        assert!(
            matches!(e, ApiError::Detailed { ref extra, .. } if extra.contains_key("blockers"))
        );
    }
}
