//! The insert-only vocabularies (migration 0042; plan B6-8): axes and their values, axis rules, dimensions and
//! their values. Readers only; the vocab writes (insert-only, 409 `value_exists`) come with their routes.
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::error::ApiResult;

/// One `axis`.
#[derive(Debug, Clone, sqlx::FromRow, Serialize)]
pub struct AxisRow {
    pub key: String,
    pub label: String,
    pub description: String,
    /// `one` or `many`.
    pub cardinality: String,
    pub object: String,
    /// `M1` or `M4`.
    pub milestone: String,
    pub created_at: DateTime<Utc>,
}

/// One `axis_value` (`key` = `parent.leaf`).
#[derive(Debug, Clone, sqlx::FromRow, Serialize)]
pub struct AxisValueRow {
    pub axis: String,
    pub key: String,
    pub label: String,
    pub parent: Option<String>,
    pub added_at: DateTime<Utc>,
    pub added_by: String,
}

/// One `axis_rule` definition with its profile layer's key.
#[derive(Debug, Clone, sqlx::FromRow, Serialize)]
pub struct AxisRuleRow {
    pub key: String,
    pub priority: i32,
    pub condition: Value,
    pub layer_id: Uuid,
    pub layer_key: String,
    /// The template family whose tags carry the rule (B6-3).
    pub owner: String,
    pub created_by: String,
    pub created_at: DateTime<Utc>,
}

/// One `dimension`.
#[derive(Debug, Clone, sqlx::FromRow, Serialize)]
pub struct DimensionRow {
    pub key: String,
    pub label: String,
    pub description: String,
    /// `dimension`, `grouping` or `placeholder`.
    pub kind: String,
    pub concerns: Option<Value>,
    pub created_at: DateTime<Utc>,
}

/// One `dimension_value`.
#[derive(Debug, Clone, sqlx::FromRow, Serialize)]
pub struct DimensionValueRow {
    pub dimension: String,
    pub key: String,
    pub label: String,
    pub note: Option<String>,
    pub added_at: DateTime<Utc>,
    pub added_by: String,
}

/// Every axis, by key.
pub async fn axes(conn: &mut PgConnection) -> ApiResult<Vec<AxisRow>> {
    Ok(sqlx::query_as::<_, AxisRow>(
        "SELECT key, label, description, cardinality, object, milestone, created_at FROM axis ORDER BY key",
    )
    .fetch_all(conn)
    .await?)
}

/// The values of one axis (or of every axis), by (axis, key).
pub async fn axis_values(
    conn: &mut PgConnection,
    axis: Option<&str>,
) -> ApiResult<Vec<AxisValueRow>> {
    Ok(sqlx::query_as::<_, AxisValueRow>(
        "SELECT axis, key, label, parent, added_at, added_by FROM axis_value \
          WHERE ($1::text IS NULL OR axis = $1) ORDER BY axis, key",
    )
    .bind(axis)
    .fetch_all(conn)
    .await?)
}

const RULE_SELECT: &str = "SELECT r.key, r.priority, r.condition, r.layer_id, l.key AS layer_key, r.owner, \
     r.created_by, r.created_at FROM axis_rule r JOIN layer l ON l.id = r.layer_id";

/// Every axis rule, by (priority, key).
pub async fn axis_rules(conn: &mut PgConnection) -> ApiResult<Vec<AxisRuleRow>> {
    Ok(
        sqlx::query_as::<_, AxisRuleRow>(sqlx::AssertSqlSafe(format!(
            "{RULE_SELECT} ORDER BY r.priority, r.key"
        )))
        .fetch_all(conn)
        .await?,
    )
}

/// One axis rule, if any.
pub async fn axis_rule(conn: &mut PgConnection, key: &str) -> ApiResult<Option<AxisRuleRow>> {
    Ok(
        sqlx::query_as::<_, AxisRuleRow>(sqlx::AssertSqlSafe(format!(
            "{RULE_SELECT} WHERE r.key = $1"
        )))
        .bind(key)
        .fetch_optional(conn)
        .await?,
    )
}

/// Every dimension, by key.
pub async fn dimensions(conn: &mut PgConnection) -> ApiResult<Vec<DimensionRow>> {
    Ok(sqlx::query_as::<_, DimensionRow>(
        "SELECT key, label, description, kind, concerns, created_at FROM dimension ORDER BY key",
    )
    .fetch_all(conn)
    .await?)
}

/// The values of one dimension (or of every dimension), by (dimension, key).
pub async fn dimension_values(
    conn: &mut PgConnection,
    dimension: Option<&str>,
) -> ApiResult<Vec<DimensionValueRow>> {
    Ok(sqlx::query_as::<_, DimensionValueRow>(
        "SELECT dimension, key, label, note, added_at, added_by FROM dimension_value \
          WHERE ($1::text IS NULL OR dimension = $1) ORDER BY dimension, key",
    )
    .bind(dimension)
    .fetch_all(conn)
    .await?)
}
