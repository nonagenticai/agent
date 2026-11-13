//! The SQL behind the business-model routes (plan B6 §5 T10, T12e, T12g): the vocab reads with the 0160 retirement
//! columns, the insert-only vocab writes (`ON CONFLICT DO NOTHING` → the route's 409), the 0160 updates, and the head
//! `profile` / `axis_rule` reads the blocker checks need. Every function runs in the caller's `agent_platform`
//! transaction.
use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::error::ApiResult;
use crate::platform::db::vocab;

/// Advisory-lock space of the business vocabulary (distinct from the family lock `0x0B6A_0007`). Taken first by
/// every write that reads or changes which axis values are in use (value add / retire, cardinality, profile save,
/// rule create), so a retirement and a profile naming the value cannot both commit.
pub const BUSINESS_LOCK_SPACE: i32 = 0x0B6B_0E00;

/// Take the business-vocabulary lock (transaction-scoped). Always before any family lock.
pub async fn lock_business(conn: &mut PgConnection) -> ApiResult<()> {
    sqlx::query("SELECT pg_advisory_xact_lock($1, 0)")
        .bind(BUSINESS_LOCK_SPACE)
        .execute(conn)
        .await?;
    Ok(())
}

/// One `axis_value` with its retirement (0160). Retired values stay in every read.
#[derive(Debug, Clone, sqlx::FromRow, Serialize)]
pub struct ValueOut {
    #[serde(skip)]
    pub axis: String,
    pub key: String,
    pub label: String,
    pub parent: Option<String>,
    pub added_at: DateTime<Utc>,
    pub added_by: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retired_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retired_by: Option<String>,
}

impl ValueOut {
    pub fn active(&self) -> bool {
        self.retired_at.is_none()
    }
}

/// The web `Axis`: the axis row and its value tree (insertion order, then key).
#[derive(Debug, Clone, Serialize)]
pub struct AxisOut {
    pub key: String,
    pub label: String,
    pub description: String,
    pub cardinality: String,
    pub object: String,
    pub milestone: String,
    pub created_at: DateTime<Utc>,
    pub values: Vec<ValueOut>,
}

async fn values(conn: &mut PgConnection, axis: Option<&str>) -> ApiResult<Vec<ValueOut>> {
    Ok(sqlx::query_as::<_, ValueOut>(
        "SELECT axis, key, label, parent, added_at, added_by, retired_at, retired_by FROM axis_value \
          WHERE ($1::text IS NULL OR axis = $1) ORDER BY axis, added_at, key",
    )
    .bind(axis)
    .fetch_all(conn)
    .await?)
}

/// Every axis with its values, by key.
pub async fn axes(conn: &mut PgConnection) -> ApiResult<Vec<AxisOut>> {
    let rows = vocab::axes(&mut *conn).await?;
    let mut by_axis: BTreeMap<String, Vec<ValueOut>> = BTreeMap::new();
    for v in values(&mut *conn, None).await? {
        by_axis.entry(v.axis.clone()).or_default().push(v);
    }
    Ok(rows
        .into_iter()
        .map(|a| {
            let mine = by_axis.remove(&a.key).unwrap_or_default();
            AxisOut {
                key: a.key,
                label: a.label,
                description: a.description,
                cardinality: a.cardinality,
                object: a.object,
                milestone: a.milestone,
                created_at: a.created_at,
                values: mine,
            }
        })
        .collect())
}

/// One axis with its values, if any.
pub async fn axis(conn: &mut PgConnection, key: &str) -> ApiResult<Option<AxisOut>> {
    let row: Option<vocab::AxisRow> = sqlx::query_as(
        "SELECT key, label, description, cardinality, object, milestone, created_at FROM axis WHERE key = $1",
    )
    .bind(key)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(a) = row else { return Ok(None) };
    let values = values(&mut *conn, Some(key)).await?;
    Ok(Some(AxisOut {
        key: a.key,
        label: a.label,
        description: a.description,
        cardinality: a.cardinality,
        object: a.object,
        milestone: a.milestone,
        created_at: a.created_at,
        values,
    }))
}

/// The axis row locked for a cardinality change (0160 grants `UPDATE (cardinality)`, which `FOR UPDATE` needs).
pub async fn lock_axis(conn: &mut PgConnection, key: &str) -> ApiResult<Option<String>> {
    Ok(
        sqlx::query_scalar("SELECT cardinality FROM axis WHERE key = $1 FOR UPDATE")
            .bind(key)
            .fetch_optional(conn)
            .await?,
    )
}

/// A new axis; false when the key exists (409 `axis_exists`).
pub struct NewAxis<'a> {
    pub key: &'a str,
    pub label: &'a str,
    pub description: &'a str,
    pub cardinality: &'a str,
    pub object: &'a str,
    pub milestone: &'a str,
}

pub async fn insert_axis(conn: &mut PgConnection, a: &NewAxis<'_>) -> ApiResult<bool> {
    let done = sqlx::query(
        "INSERT INTO axis (key, label, description, cardinality, object, milestone) \
         VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (key) DO NOTHING",
    )
    .bind(a.key)
    .bind(a.label)
    .bind(a.description)
    .bind(a.cardinality)
    .bind(a.object)
    .bind(a.milestone)
    .execute(conn)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// A new axis value; false when (axis, key) exists, retired or not (409 `value_exists`: insert-only).
pub async fn insert_axis_value(
    conn: &mut PgConnection,
    axis: &str,
    key: &str,
    label: &str,
    parent: Option<&str>,
    by: &str,
) -> ApiResult<bool> {
    let done = sqlx::query(
        "INSERT INTO axis_value (axis, key, label, parent, added_by) VALUES ($1, $2, $3, $4, $5) \
         ON CONFLICT (axis, key) DO NOTHING",
    )
    .bind(axis)
    .bind(key)
    .bind(label)
    .bind(parent)
    .bind(by)
    .execute(conn)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// Retire one value (0160; the trigger refuses a second retirement, which the route checks first).
pub async fn retire_axis_value(
    conn: &mut PgConnection,
    axis: &str,
    key: &str,
    by: &str,
) -> ApiResult<()> {
    sqlx::query(
        "UPDATE axis_value SET retired_at = now(), retired_by = $3 \
          WHERE axis = $1 AND key = $2 AND retired_at IS NULL",
    )
    .bind(axis)
    .bind(key)
    .bind(by)
    .execute(conn)
    .await?;
    Ok(())
}

pub async fn set_cardinality(conn: &mut PgConnection, axis: &str, to: &str) -> ApiResult<()> {
    sqlx::query("UPDATE axis SET cardinality = $2 WHERE key = $1")
        .bind(axis)
        .bind(to)
        .execute(conn)
        .await?;
    Ok(())
}

/// A current, not tombstoned `profile` row with its layer.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ProfileRow {
    pub layer_key: String,
    pub key: String,
    pub rev: i32,
    pub fields: Value,
    pub author: String,
    pub created_at: DateTime<Utc>,
}

const PROFILE_SELECT: &str = "SELECT l.key AS layer_key, r.key, r.rev, r.fields, r.author, r.created_at \
       FROM layer_row r JOIN layer l ON l.id = r.layer_id \
      WHERE r.kind = 'profile' AND r.is_current AND r.op <> 'tombstone'";

/// Every head business profile, by (key, layer).
pub async fn profiles(conn: &mut PgConnection) -> ApiResult<Vec<ProfileRow>> {
    Ok(sqlx::query_as::<_, ProfileRow>(sqlx::AssertSqlSafe(format!(
        "{PROFILE_SELECT} ORDER BY r.key, l.key"
    )))
    .fetch_all(conn)
    .await?)
}

/// The head profiles of one key (normally one; a key in two layers is listed so the save can settle it).
pub async fn profiles_of(conn: &mut PgConnection, key: &str) -> ApiResult<Vec<ProfileRow>> {
    Ok(sqlx::query_as::<_, ProfileRow>(sqlx::AssertSqlSafe(format!(
        "{PROFILE_SELECT} AND r.key = $1 ORDER BY l.key"
    )))
    .bind(key)
    .fetch_all(conn)
    .await?)
}

/// The head profiles whose selection names `value` on `axis`.
pub async fn profiles_holding(
    conn: &mut PgConnection,
    axis: &str,
    value: &str,
) -> ApiResult<Vec<ProfileRow>> {
    Ok(sqlx::query_as::<_, ProfileRow>(sqlx::AssertSqlSafe(format!(
        "{PROFILE_SELECT} AND jsonb_typeof(r.fields -> 'selection' -> $1::text) = 'array' \
               AND (r.fields -> 'selection' -> $1::text) ? $2::text ORDER BY r.key, l.key"
    )))
    .bind(axis)
    .bind(value)
    .fetch_all(conn)
    .await?)
}

/// The head profiles holding more than one value on `axis` (block many → one).
pub async fn profiles_with_many(conn: &mut PgConnection, axis: &str) -> ApiResult<Vec<ProfileRow>> {
    Ok(
        sqlx::query_as::<_, ProfileRow>(sqlx::AssertSqlSafe(format!(
            "{PROFILE_SELECT} AND jsonb_typeof(r.fields -> 'selection' -> $1::text) = 'array' \
               AND jsonb_array_length(r.fields -> 'selection' -> $1::text) > 1 ORDER BY r.key, l.key"
        )))
        .bind(axis)
        .fetch_all(conn)
        .await?,
    )
}

/// Every rule condition to check a value against: the `axis_rule` definitions and the head `axis_rule` rows (a
/// row patched through `writeLayerRow` may differ from its definition), as (rule key, condition JSON).
pub async fn rule_conditions(conn: &mut PgConnection) -> ApiResult<Vec<(String, Value)>> {
    Ok(sqlx::query_as(
        "SELECT key, condition FROM axis_rule \
         UNION ALL \
         SELECT key, fields -> 'condition' FROM layer_row \
          WHERE kind = 'axis_rule' AND is_current AND op <> 'tombstone' AND fields ? 'condition' \
         ORDER BY 1",
    )
    .fetch_all(conn)
    .await?)
}

/// The web `AxisRule` plus its owner (D99) and profile layer key.
#[derive(Debug, Clone, sqlx::FromRow, Serialize)]
pub struct RuleOut {
    pub key: String,
    pub priority: i32,
    pub condition: Value,
    /// Entity keys the profile layer adds.
    pub adds: Vec<String>,
    /// Current rows of the profile layer.
    pub rows: i32,
    pub layer_id: Uuid,
    /// `profile/<key>`.
    pub layer_key: String,
    /// The template family whose tags carry the rule (`base` or an industry, D99).
    pub owner: String,
    pub created_by: String,
    pub created_at: DateTime<Utc>,
}

const RULE_SELECT: &str = "SELECT r.key, r.priority, r.condition, \
        coalesce((SELECT array_agg(x.key ORDER BY x.key) FROM layer_row x \
                   WHERE x.layer_id = r.layer_id AND x.is_current AND x.kind = 'entity' \
                     AND x.op <> 'tombstone'), '{}') AS adds, \
        (SELECT count(*) FROM layer_row x WHERE x.layer_id = r.layer_id AND x.is_current)::int AS rows, \
        r.layer_id, l.key AS layer_key, r.owner, r.created_by, r.created_at \
   FROM axis_rule r JOIN layer l ON l.id = r.layer_id";

/// Every rule, by (priority, key).
pub async fn rules(conn: &mut PgConnection) -> ApiResult<Vec<RuleOut>> {
    Ok(sqlx::query_as::<_, RuleOut>(sqlx::AssertSqlSafe(format!(
        "{RULE_SELECT} ORDER BY r.priority, r.key"
    )))
    .fetch_all(conn)
    .await?)
}

pub async fn rule(conn: &mut PgConnection, key: &str) -> ApiResult<Option<RuleOut>> {
    Ok(sqlx::query_as::<_, RuleOut>(sqlx::AssertSqlSafe(format!(
        "{RULE_SELECT} WHERE r.key = $1"
    )))
    .bind(key)
    .fetch_optional(conn)
    .await?)
}

pub async fn rule_exists(conn: &mut PgConnection, key: &str) -> ApiResult<bool> {
    Ok(
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM axis_rule WHERE key = $1)")
            .bind(key)
            .fetch_one(conn)
            .await?,
    )
}

pub async fn insert_rule(
    conn: &mut PgConnection,
    key: &str,
    priority: i32,
    condition: &Value,
    layer_id: Uuid,
    owner: &str,
    by: &str,
) -> ApiResult<()> {
    sqlx::query(
        "INSERT INTO axis_rule (key, priority, condition, layer_id, owner, created_by) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(key)
    .bind(priority)
    .bind(condition)
    .bind(layer_id)
    .bind(owner)
    .bind(by)
    .execute(conn)
    .await?;
    Ok(())
}

/// The objects an axis can classify: head `object` rows (PascalCase of their key: `access_grant` → `AccessGrant`)
/// and head `entity` rows (rule-added objects such as `Renewal`).
pub async fn catalogue_objects(conn: &mut PgConnection) -> ApiResult<BTreeSet<String>> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT kind, key FROM layer_row \
          WHERE kind IN ('object', 'entity') AND is_current AND op <> 'tombstone'",
    )
    .fetch_all(conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(kind, key)| if kind == "object" { pascal(&key) } else { key })
        .collect())
}

/// `access_grant` → `AccessGrant`.
pub fn pascal(snake: &str) -> String {
    snake
        .split('_')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let mut c = p.chars();
            match c.next() {
                Some(f) => f.to_ascii_uppercase().to_string() + c.as_str(),
                None => String::new(),
            }
        })
        .collect()
}

/// One `dimension_value`.
#[derive(Debug, Clone, sqlx::FromRow, Serialize)]
pub struct DimValueOut {
    #[serde(skip)]
    pub dimension: String,
    pub key: String,
    pub label: String,
    pub added_at: DateTime<Utc>,
    pub added_by: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// The web `Dimension`.
#[derive(Debug, Clone, Serialize)]
pub struct DimensionOut {
    pub key: String,
    pub label: String,
    pub description: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub concerns: Option<Value>,
    pub created_at: DateTime<Utc>,
    pub values: Vec<DimValueOut>,
}

async fn dim_values(conn: &mut PgConnection, dim: Option<&str>) -> ApiResult<Vec<DimValueOut>> {
    Ok(sqlx::query_as::<_, DimValueOut>(
        "SELECT dimension, key, label, added_at, added_by, note FROM dimension_value \
          WHERE ($1::text IS NULL OR dimension = $1) ORDER BY dimension, added_at, key",
    )
    .bind(dim)
    .fetch_all(conn)
    .await?)
}

fn dimension_out(d: vocab::DimensionRow, values: Vec<DimValueOut>) -> DimensionOut {
    DimensionOut {
        key: d.key,
        label: d.label,
        description: d.description,
        kind: d.kind,
        concerns: d.concerns,
        created_at: d.created_at,
        values,
    }
}

/// Every dimension with its values, by key.
pub async fn dimensions(conn: &mut PgConnection) -> ApiResult<Vec<DimensionOut>> {
    let rows = vocab::dimensions(&mut *conn).await?;
    let mut by_dim: BTreeMap<String, Vec<DimValueOut>> = BTreeMap::new();
    for v in dim_values(&mut *conn, None).await? {
        by_dim.entry(v.dimension.clone()).or_default().push(v);
    }
    Ok(rows
        .into_iter()
        .map(|d| {
            let mine = by_dim.remove(&d.key).unwrap_or_default();
            dimension_out(d, mine)
        })
        .collect())
}

pub async fn dimension(conn: &mut PgConnection, key: &str) -> ApiResult<Option<DimensionOut>> {
    let row: Option<vocab::DimensionRow> = sqlx::query_as(
        "SELECT key, label, description, kind, concerns, created_at FROM dimension WHERE key = $1",
    )
    .bind(key)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(d) = row else { return Ok(None) };
    let values = dim_values(&mut *conn, Some(key)).await?;
    Ok(Some(dimension_out(d, values)))
}

/// A new dimension; false when the key exists (409 `dimension_exists`).
pub async fn insert_dimension(
    conn: &mut PgConnection,
    key: &str,
    label: &str,
    description: &str,
    kind: &str,
) -> ApiResult<bool> {
    let done = sqlx::query(
        "INSERT INTO dimension (key, label, description, kind) VALUES ($1, $2, $3, $4) \
         ON CONFLICT (key) DO NOTHING",
    )
    .bind(key)
    .bind(label)
    .bind(description)
    .bind(kind)
    .execute(conn)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// A new dimension value; false when it exists (409 `value_exists`: insert-only).
pub async fn insert_dimension_value(
    conn: &mut PgConnection,
    dimension: &str,
    key: &str,
    label: &str,
    note: Option<&str>,
    by: &str,
) -> ApiResult<bool> {
    let done = sqlx::query(
        "INSERT INTO dimension_value (dimension, key, label, note, added_by) VALUES ($1, $2, $3, $4, $5) \
         ON CONFLICT (dimension, key) DO NOTHING",
    )
    .bind(dimension)
    .bind(key)
    .bind(label)
    .bind(note)
    .bind(by)
    .execute(conn)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// One `renewal_preset` (seed only).
#[derive(Debug, Clone, sqlx::FromRow, Serialize)]
pub struct RenewalPresetOut {
    pub key: String,
    pub label: String,
    pub example: String,
    pub order_type: String,
    pub initiator: String,
    pub mode: Option<String>,
}

pub async fn renewal_presets(conn: &mut PgConnection) -> ApiResult<Vec<RenewalPresetOut>> {
    Ok(sqlx::query_as::<_, RenewalPresetOut>(
        "SELECT key, label, example, order_type, initiator, mode FROM renewal_preset ORDER BY key",
    )
    .fetch_all(conn)
    .await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pascal_case() {
        assert_eq!(pascal("access_grant"), "AccessGrant");
        assert_eq!(pascal("order"), "Order");
        assert_eq!(pascal("service_event"), "ServiceEvent");
    }
}
