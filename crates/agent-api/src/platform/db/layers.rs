//! Layers (`layer`, migration 0041): lookups, the family list and the per-layer summaries of P4.
use agent_model::layer::Scope;
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

use super::corrupt;
use crate::error::{ApiError, ApiResult};

/// One `layer` row. `key` is the web `layerLabel`; `family` is the template layer whose tag carries it (itself
/// for a head).
#[derive(Debug, Clone, sqlx::FromRow, Serialize)]
pub struct Layer {
    pub id: Uuid,
    pub key: String,
    pub scope: String,
    pub industry: Option<String>,
    pub department: Option<String>,
    pub job: Option<String>,
    pub rule: Option<String>,
    pub dimension_key: Option<String>,
    pub dimension_value: Option<String>,
    /// D111 (0139): the region of a region sub-layer (`…/region/<r>`); its `family` is its parent's key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    pub family: String,
    pub label: Option<String>,
    pub rollout_wave: Option<i16>,
    pub wave_label: Option<String>,
    pub extends: Option<String>,
    pub areas: Vec<String>,
    pub template_selection: Option<Value>,
    pub created_at: DateTime<Utc>,
    /// 0230: an industry or department taken back from the tenant choices (pins keep compiling).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retired_at: Option<DateTime<Utc>>,
}

/// The columns of [`Layer`], in order.
pub(crate) const LAYER_COLUMNS: &str = "id, key, scope, industry, department, job, rule, dimension_key, \
     dimension_value, region, family, label, rollout_wave, wave_label, extends, areas, template_selection, created_at, \
     retired_at";

impl Layer {
    /// The scope as the agent-model enum (the schema's CHECK guarantees it parses).
    pub fn parsed_scope(&self) -> ApiResult<Scope> {
        Scope::parse(&self.scope).ok_or_else(|| {
            corrupt(format!(
                "layer {} has scope {:?}, which is not a scope",
                self.key, self.scope
            ))
        })
    }

    /// A template layer (base, industry, base_dept, industry_dept, job without a dimension): its own family.
    pub fn is_head(&self) -> bool {
        self.family == self.key
    }

    /// The industry a template layer composes in (web `layerStack`): the layer's own industry (industry and
    /// industry_dept layers), else the `industry` of its `template_selection` (job heads); none for base and
    /// base_dept (D116: a base_dept carries neither an industry nor a template_selection).
    pub fn template_industry(&self) -> Option<&str> {
        self.industry
            .as_deref()
            .or_else(|| self.template_selection.as_ref()?.get("industry")?.as_str())
    }
}

fn not_found(what: impl std::fmt::Display) -> ApiError {
    ApiError::Missing("layer_not_found", format!("No layer “{what}”."))
}

/// The layer with this key, if any.
pub async fn find_layer_by_key(conn: &mut PgConnection, key: &str) -> ApiResult<Option<Layer>> {
    Ok(sqlx::query_as::<_, Layer>(sqlx::AssertSqlSafe(format!(
        "SELECT {LAYER_COLUMNS} FROM layer WHERE key = $1"
    )))
    .bind(key)
    .fetch_optional(conn)
    .await?)
}

/// The layer with this key; 404 `layer_not_found`.
pub async fn layer_by_key(conn: &mut PgConnection, key: &str) -> ApiResult<Layer> {
    find_layer_by_key(conn, key)
        .await?
        .ok_or_else(|| not_found(key))
}

/// The layer with this id; 404 `layer_not_found`.
pub async fn layer_by_id(conn: &mut PgConnection, id: Uuid) -> ApiResult<Layer> {
    sqlx::query_as::<_, Layer>(sqlx::AssertSqlSafe(format!(
        "SELECT {LAYER_COLUMNS} FROM layer WHERE id = $1"
    )))
    .bind(id)
    .fetch_optional(conn)
    .await?
    .ok_or_else(|| not_found(id))
}

/// Every layer, by key.
pub async fn layers(conn: &mut PgConnection) -> ApiResult<Vec<Layer>> {
    Ok(sqlx::query_as::<_, Layer>(sqlx::AssertSqlSafe(format!(
        "SELECT {LAYER_COLUMNS} FROM layer ORDER BY key"
    )))
    .fetch_all(conn)
    .await?)
}

/// The layers of one family (the head, its job×dimension layers, the profile layers of the rules it owns), by
/// key. Empty when there is no such family.
pub async fn family_layers(conn: &mut PgConnection, family: &str) -> ApiResult<Vec<Layer>> {
    Ok(sqlx::query_as::<_, Layer>(sqlx::AssertSqlSafe(format!(
        "SELECT {LAYER_COLUMNS} FROM layer WHERE family = $1 ORDER BY key"
    )))
    .bind(family)
    .fetch_all(conn)
    .await?)
}

/// A template family: its head, the keys of its layers, its latest non-provisional tag and how many of its
/// current rows that tag does not hold (B6-5).
#[derive(Debug, Clone, sqlx::FromRow, Serialize)]
pub struct Family {
    pub key: String,
    pub scope: String,
    pub label: Option<String>,
    pub layers: Vec<String>,
    pub latest_tag: Option<i32>,
    pub untagged: i64,
}

/// Every family, by head key.
pub async fn families(conn: &mut PgConnection) -> ApiResult<Vec<Family>> {
    Ok(sqlx::query_as::<_, Family>(
        "SELECT h.key, h.scope, h.label, \
                ARRAY(SELECT m.key FROM layer m WHERE m.family = h.key ORDER BY m.key) AS layers, \
                (SELECT max(t.version) FROM layer_tag t \
                  WHERE t.family = h.key AND NOT t.provisional) AS latest_tag, \
                (SELECT count(*) FROM layer_untagged u WHERE u.family = h.key) AS untagged \
           FROM layer h \
          WHERE h.family = h.key \
          ORDER BY h.key",
    )
    .fetch_all(conn)
    .await?)
}

/// P4's per-layer rollup (web `LayerSummary`, plus the layer key, family and the family's latest tag).
/// `untagged` follows B6-5 (current rows not in the family's latest non-provisional tag), not the fixture's
/// `created_at > tagged_at` approximation.
#[derive(Debug, Clone, sqlx::FromRow, Serialize)]
pub struct LayerSummary {
    pub layer_id: Uuid,
    pub layer_key: String,
    pub family: String,
    /// Current revisions by op.
    pub adds: i64,
    pub patches: i64,
    pub tombstones: i64,
    /// Superseded revisions kept for history.
    pub retired: i64,
    /// Distinct authors of current revisions, most recent first.
    pub authors: Vec<String>,
    pub last_change_at: Option<DateTime<Utc>>,
    pub untagged: i64,
    /// The family's latest non-provisional tag version.
    pub latest_tag: Option<i32>,
}

/// The summary of every layer (or of the layers of one family), by layer key.
pub async fn layer_summaries(
    conn: &mut PgConnection,
    family: Option<&str>,
) -> ApiResult<Vec<LayerSummary>> {
    Ok(sqlx::query_as::<_, LayerSummary>(
        "SELECT l.id AS layer_id, l.key AS layer_key, l.family, \
                count(r.id) FILTER (WHERE r.is_current AND r.op = 'add') AS adds, \
                count(r.id) FILTER (WHERE r.is_current AND r.op = 'patch') AS patches, \
                count(r.id) FILTER (WHERE r.is_current AND r.op = 'tombstone') AS tombstones, \
                count(r.id) FILTER (WHERE NOT r.is_current) AS retired, \
                ARRAY(SELECT a.author FROM layer_row a WHERE a.layer_id = l.id AND a.is_current \
                       GROUP BY a.author ORDER BY max(a.created_at) DESC, a.author) AS authors, \
                max(r.created_at) FILTER (WHERE r.is_current) AS last_change_at, \
                (SELECT count(*) FROM layer_untagged u WHERE u.layer_id = l.id) AS untagged, \
                (SELECT max(t.version) FROM layer_tag t \
                  WHERE t.family = l.family AND NOT t.provisional) AS latest_tag \
           FROM layer l \
           LEFT JOIN layer_row r ON r.layer_id = l.id \
          WHERE ($1::text IS NULL OR l.family = $1) \
          GROUP BY l.id, l.key, l.family \
          ORDER BY l.key",
    )
    .bind(family)
    .fetch_all(conn)
    .await?)
}
