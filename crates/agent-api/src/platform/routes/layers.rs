//! B6b platform-model area `layers` (plan B6 §5, T9a): the six P4 layer reads, and T12b raw authoring (F-21; the
//! KH bucket of Revision 2026-10-22, on KH's region grammar and row refusals):
//!
//! - `POST /api/layers` (createLayer) → 201 `Layer`: key from `agent_model::layer` (region sub-layers
//!   `…/region/<r>` of base / industry / industry_dept, family = the parent, D111); 422 `invalid_layer_key`,
//!   `invalid_label`; 404 `parent_layer_not_found` (parent layer, or an industry_dept's `extends` archetype, D107);
//!   409 `layer_exists`; one `layer.create` platform audit row. A new layer has no tag until `tagLayerRevisions`.
//! - `POST /api/layers/{id}/rows` (writeLayerRow) → 201 `LayerRow`: `{kind, key, op, fields, expected_rev?}` through
//!   `db::authored::write_authored` (409 `duplicate_add`, `add_tombstoned`, `stale_revision`; 404 `row_not_found`)
//!   and `write_revision` (422 `kind_not_allowed_in_layer`, `invalid_key`, `invalid_row`, `pack_kind_undeclared`);
//!   its `layer.revision` audit row is the one row of the write. Retire = `op: tombstone`.
//!
//! - `GET /api/layers` (layerCatalog) → `Paged<Layer>`, by key.
//! - `GET /api/layers/summary` (layerSummaries) → `LayerSummary[]`, by layer key.
//! - `GET /api/layers/{id}/rows?include_retired=` (layerRows) → `Paged<LayerRow>` by (kind, key, rev newest first);
//!   404 `layer_not_found`.
//! - `GET /api/layers/{id}/rows/{kind}/{key}/revisions` (layerRowRevisions) → `LayerRow[]`, newest first; 404
//!   `layer_not_found`, 422 `invalid_row` (not a row kind).
//! - `GET /api/layers/{id}/overrides` (layerOverrideTargets) → `LayerOverride[]`; 404 `layer_not_found`.
//! - `GET /api/layer-rows/{row_id}` (findLayerRow) → `{layer, row}`; 404 `row_not_found`.
//!
//! P4 shows the configurator's overlay kinds only (§10: `layerRows`, `layerSummaries` and the override targets
//! count overlay rows; catalogue kinds are reviewed on P6/P7/P9–P15). `findLayerRow` and `layerRowRevisions` read
//! any kind (a deep link names its row). Every handler is `SuperAdmin`, one `REPEATABLE READ READ ONLY` snapshot.
//!
//! The UI `Layer` (web `types/platform.ts`) is [`LayerView`]: the scope keys, `dimension: [key, value]`, `extends`,
//! and `tag` = `<family>@v<n>` of the family's latest non-provisional tag with its `tagged_at` (B6-5), plus the
//! layer `key` and `family`.
use std::collections::{HashMap, HashSet};

use agent_model::kinds::Kind;
use agent_model::layer::{LayerKey, Scope};
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use super::{Who, audit, missing, parse_kind, parse_op, read_tx, unprocessable, write_tx};
use crate::audit::PlatformEvent;
use crate::error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult};
use crate::page::{flag, paged};
use crate::platform::db::authored::{Expect, LayerMeta, insert_layer, write_authored};
use crate::platform::db::layers::{self, Layer, LayerSummary};
use crate::platform::db::rows::{self, Row};
use crate::state::AppState;
use crate::tenancy::SuperAdmin;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/layers", get(layer_catalog).post(create_layer))
        .route("/api/layers/summary", get(layer_summaries))
        .route(
            "/api/layers/{id}/rows",
            get(layer_rows).post(write_layer_row),
        )
        .route(
            "/api/layers/{id}/rows/{kind}/{key}/revisions",
            get(layer_row_revisions),
        )
        .route("/api/layers/{id}/overrides", get(layer_override_targets))
        .route("/api/layer-rows/{row_id}", get(find_layer_row))
}

/// The UI `Layer`.
#[derive(Debug, Clone, Serialize)]
pub struct LayerView {
    pub id: Uuid,
    pub key: String,
    pub family: String,
    pub scope: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub industry: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub department: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rule: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dimension: Option<[String; 2]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extends: Option<String>,
    /// `<family>@v<n>`: the family's latest non-provisional tag; null when it has none.
    pub tag: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tagged_at: Option<DateTime<Utc>>,
}

/// Family → (latest non-provisional version, its `tagged_at`).
pub type LatestTags = HashMap<String, (i32, DateTime<Utc>)>;

impl LayerView {
    pub fn of(l: &Layer, tags: &LatestTags) -> LayerView {
        let latest = tags.get(&l.family);
        let dimension = match (&l.dimension_key, &l.dimension_value) {
            (Some(k), Some(v)) => Some([k.clone(), v.clone()]),
            _ => None,
        };
        LayerView {
            id: l.id,
            key: l.key.clone(),
            family: l.family.clone(),
            scope: l.scope.clone(),
            industry: l.industry.clone(),
            department: l.department.clone(),
            job: l.job.clone(),
            rule: l.rule.clone(),
            dimension,
            extends: l.extends.clone(),
            tag: latest.map(|(v, _)| format!("{}@v{v}", l.family)),
            tagged_at: latest.map(|(_, at)| *at),
        }
    }
}

/// The latest non-provisional tag of every family that has one.
pub async fn latest_tags(conn: &mut PgConnection) -> ApiResult<LatestTags> {
    let rows: Vec<(String, i32, DateTime<Utc>)> = sqlx::query_as(
        "SELECT DISTINCT ON (family) family, version, tagged_at FROM layer_tag \
          WHERE NOT provisional ORDER BY family, version DESC",
    )
    .fetch_all(conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(family, version, at)| (family, (version, at)))
        .collect())
}

/// The configurator's overlay kinds (UI `RowKind`), as stored.
pub fn overlay_kinds() -> Vec<String> {
    Kind::ALL
        .into_iter()
        .filter(|k| k.is_overlay())
        .map(|k| k.as_str().to_string())
        .collect()
}

fn is_overlay(kind: &str) -> bool {
    Kind::parse(kind).is_some_and(Kind::is_overlay)
}

/// A layer id from the path: not a uuid → 404 `layer_not_found` (no such layer either way).
fn layer_id(raw: &str) -> ApiResult<Uuid> {
    Uuid::parse_str(raw).map_err(|_| missing("layer_not_found", format!("No layer “{raw}”.")))
}

fn row_view(r: &Row) -> Value {
    json!(r)
}

async fn layer_catalog(
    SuperAdmin(_c): SuperAdmin,
    State(st): State<AppState>,
) -> ApiResult<Json<Value>> {
    let mut tx = read_tx(&st).await?;
    let tags = latest_tags(&mut tx).await?;
    let items = layers::layers(&mut tx)
        .await?
        .iter()
        .map(|l| json!(LayerView::of(l, &tags)))
        .collect();
    Ok(Json(paged(items, None)))
}

/// P4's per-layer rollup over overlay rows (§10). Same shape and semantics as `db::layers::layer_summaries`
/// (B6-5 `untagged`), restricted to the overlay kinds.
const OVERLAY_SUMMARIES: &str = "SELECT l.id AS layer_id, l.key AS layer_key, l.family, \
        count(r.id) FILTER (WHERE r.is_current AND r.op = 'add') AS adds, \
        count(r.id) FILTER (WHERE r.is_current AND r.op = 'patch') AS patches, \
        count(r.id) FILTER (WHERE r.is_current AND r.op = 'tombstone') AS tombstones, \
        count(r.id) FILTER (WHERE NOT r.is_current) AS retired, \
        ARRAY(SELECT a.author FROM layer_row a \
               WHERE a.layer_id = l.id AND a.is_current AND a.kind = ANY($1) \
               GROUP BY a.author ORDER BY max(a.created_at) DESC, a.author) AS authors, \
        max(r.created_at) FILTER (WHERE r.is_current) AS last_change_at, \
        (SELECT count(*) FROM layer_untagged u WHERE u.layer_id = l.id AND u.kind = ANY($1)) AS untagged, \
        (SELECT max(t.version) FROM layer_tag t \
          WHERE t.family = l.family AND NOT t.provisional) AS latest_tag \
   FROM layer l \
   LEFT JOIN layer_row r ON r.layer_id = l.id AND r.kind = ANY($1) \
  GROUP BY l.id, l.key, l.family \
  ORDER BY l.key";

async fn layer_summaries(
    SuperAdmin(_c): SuperAdmin,
    State(st): State<AppState>,
) -> ApiResult<Json<Vec<LayerSummary>>> {
    let mut tx = read_tx(&st).await?;
    let list = sqlx::query_as::<_, LayerSummary>(OVERLAY_SUMMARIES)
        .bind(overlay_kinds())
        .fetch_all(&mut *tx)
        .await?;
    Ok(Json(list))
}

#[derive(Deserialize)]
struct RowsQuery {
    include_retired: Option<String>,
}

async fn layer_rows(
    SuperAdmin(_c): SuperAdmin,
    State(st): State<AppState>,
    ApiPath(id): ApiPath<String>,
    ApiQuery(q): ApiQuery<RowsQuery>,
) -> ApiResult<Json<Value>> {
    let id = layer_id(&id)?;
    let retired = flag(q.include_retired.as_deref(), "include_retired")?;
    let mut tx = read_tx(&st).await?;
    let mut list: Vec<Row> = rows::layer_rows(&mut tx, id, None, retired)
        .await?
        .into_iter()
        .filter(|r| is_overlay(&r.kind))
        .collect();
    // The fixture order: kind, key, newest revision first (bytewise, not by SQL collation).
    list.sort_by(|a, b| {
        (a.kind.as_str(), a.key.as_str())
            .cmp(&(b.kind.as_str(), b.key.as_str()))
            .then(b.rev.cmp(&a.rev))
    });
    Ok(Json(paged(list.iter().map(row_view).collect(), None)))
}

async fn layer_row_revisions(
    SuperAdmin(_c): SuperAdmin,
    State(st): State<AppState>,
    ApiPath((id, kind, key)): ApiPath<(String, String, String)>,
) -> ApiResult<Json<Vec<Row>>> {
    let id = layer_id(&id)?;
    let kind = parse_kind(&kind)?;
    let mut tx = read_tx(&st).await?;
    let mut list = rows::revisions(&mut tx, id, kind, &key).await?;
    list.reverse();
    Ok(Json(list))
}

async fn find_layer_row(
    SuperAdmin(_c): SuperAdmin,
    State(st): State<AppState>,
    ApiPath(row_id): ApiPath<String>,
) -> ApiResult<Json<Value>> {
    let not_found = || -> ApiError { missing("row_not_found", "No layer row with that id.") };
    let row_id = Uuid::parse_str(&row_id).map_err(|_| not_found())?;
    let mut tx = read_tx(&st).await?;
    let row = rows::row_by_id(&mut tx, row_id).await?;
    let layer = layers::layer_by_id(&mut tx, row.layer_id).await?;
    let tags = latest_tags(&mut tx).await?;
    Ok(Json(
        json!({"layer": LayerView::of(&layer, &tags), "row": row_view(&row)}),
    ))
}

// ---- T12b authoring ------------------------------------------------------------------------------------------------

/// `POST /api/layers` body (plan §5 createLayer, D111 `region`; the B6-9 metadata a template head carries).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateLayerBody {
    pub scope: String,
    pub industry: Option<String>,
    pub department: Option<String>,
    pub job: Option<String>,
    pub dimension: Option<[String; 2]>,
    pub region: Option<String>,
    pub label: String,
    pub extends: Option<String>,
    #[serde(default)]
    pub areas: Vec<String>,
    pub rollout_wave: Option<i16>,
    pub wave_label: Option<String>,
    pub template_selection: Option<Value>,
}

/// The layer key a create body names; 422 `invalid_layer_key` for an unknown scope, a profile layer (created by
/// `createAxisRule`, which owns its rule) or a key the scope does not take (`job/…/region/…` included).
pub fn key_of(b: &CreateLayerBody) -> ApiResult<LayerKey> {
    let scope = Scope::parse(&b.scope).ok_or_else(|| {
        unprocessable(
            "invalid_layer_key",
            format!(
                "“{}” is not a layer scope (base, industry, industry_dept, job).",
                b.scope
            ),
        )
    })?;
    if scope == Scope::Profile {
        return Err(unprocessable(
            "invalid_layer_key",
            "A profile layer is created with its axis rule (POST /api/axis-rules).",
        ));
    }
    let mut key = LayerKey::new(scope);
    key.industry = b.industry.clone();
    key.department = b.department.clone();
    key.job = b.job.clone();
    key.dimension = b.dimension.clone().map(|[k, v]| (k, v));
    key.region = b.region.clone();
    key.validate()
        .map_err(|e| unprocessable("invalid_layer_key", format!("Not a layer key: {e}.")))?;
    Ok(key)
}

async fn create_layer(
    sa: SuperAdmin,
    State(st): State<AppState>,
    ApiJson(b): ApiJson<CreateLayerBody>,
) -> ApiResult<(StatusCode, Json<LayerView>)> {
    let who = Who::of(&sa);
    let key = key_of(&b)?;
    super::check_label(&b.label)?;
    let meta = LayerMeta {
        label: Some(b.label.clone()),
        rollout_wave: b.rollout_wave,
        wave_label: b.wave_label.clone(),
        extends: b.extends.clone(),
        areas: b.areas.clone(),
        template_selection: b.template_selection.clone(),
    };
    let mut tx = write_tx(&st).await?;
    let layer = insert_layer(&mut tx, &key, None, &meta).await?;
    audit(
        &mut tx,
        &who,
        PlatformEvent::new("layer.create", layer.key.as_str())
            .detail("layer", layer.key.as_str())
            .detail("family", layer.family.as_str())
            .detail("scope", layer.scope.as_str()),
    )
    .await?;
    let tags = latest_tags(&mut tx).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(LayerView::of(&layer, &tags))))
}

/// `POST /api/layers/{id}/rows` body. `expected_rev` is required for `patch` / `tombstone`, refused for `add`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WriteRowBody {
    pub kind: String,
    pub key: String,
    pub op: String,
    #[serde(default)]
    pub fields: Option<Value>,
    pub expected_rev: Option<i32>,
}

async fn write_layer_row(
    sa: SuperAdmin,
    State(st): State<AppState>,
    ApiPath(id): ApiPath<String>,
    ApiJson(b): ApiJson<WriteRowBody>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let who = Who::of(&sa);
    let id = layer_id(&id)?;
    let kind = parse_kind(&b.kind)?;
    let op = parse_op(&b.op)?;
    let expect = Expect::for_op(op, b.expected_rev)?;
    let fields = b.fields.unwrap_or_else(|| json!({}));
    let mut tx = write_tx(&st).await?;
    let layer = layers::layer_by_id(&mut tx, id).await?;
    let row = write_authored(
        &mut tx, &who.actor, &layer.key, kind, &b.key, op, fields, &who.by, expect,
    )
    .await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(row_view(&row))))
}

/// The UI `LayerOverride`.
#[derive(Debug, Clone, Serialize)]
pub struct OverrideView {
    pub row_id: Uuid,
    pub target: Option<LayerView>,
    pub target_row_id: Option<Uuid>,
}

async fn layer_override_targets(
    SuperAdmin(_c): SuperAdmin,
    State(st): State<AppState>,
    ApiPath(id): ApiPath<String>,
) -> ApiResult<Json<Vec<OverrideView>>> {
    let id = layer_id(&id)?;
    let mut tx = read_tx(&st).await?;
    let targets = rows::override_targets(&mut tx, id).await?;
    let overlay: HashSet<Uuid> = sqlx::query_scalar(
        "SELECT id FROM layer_row WHERE layer_id = $1 AND op <> 'add' AND kind = ANY($2)",
    )
    .bind(id)
    .bind(overlay_kinds())
    .fetch_all(&mut *tx)
    .await?
    .into_iter()
    .collect();
    let tags = latest_tags(&mut tx).await?;
    Ok(Json(
        targets
            .into_iter()
            .filter(|t| overlay.contains(&t.row_id))
            .map(|t| OverrideView {
                row_id: t.row_id,
                target: t.target.as_ref().map(|l| LayerView::of(l, &tags)),
                target_row_id: t.target_row_id,
            })
            .collect(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlay_kinds_are_the_ui_row_kinds() {
        let mut got = overlay_kinds();
        got.sort();
        assert_eq!(
            got,
            [
                "attribute",
                "connector_slot",
                "context_space",
                "entity",
                "job",
                "knowledge_store",
                "procedure",
                "relation",
            ]
        );
        assert!(is_overlay("entity"));
        assert!(!is_overlay("gate_question"));
        assert!(!is_overlay("nope"));
    }

    fn body(scope: &str) -> CreateLayerBody {
        CreateLayerBody {
            scope: scope.into(),
            industry: None,
            department: None,
            job: None,
            dimension: None,
            region: None,
            label: "L".into(),
            extends: None,
            areas: vec![],
            rollout_wave: None,
            wave_label: None,
            template_selection: None,
        }
    }

    #[test]
    fn create_keys() {
        let mut b = body("industry_dept");
        b.industry = Some("retail".into());
        b.department = Some("returns".into());
        assert_eq!(key_of(&b).unwrap().label(), "industry_dept/retail/returns");
        b.region = Some("nl".into());
        assert_eq!(
            key_of(&b).unwrap().label(),
            "industry_dept/retail/returns/region/nl"
        );
        let mut j = body("job");
        j.job = Some("care".into());
        j.dimension = Some(["channel".into(), "email".into()]);
        assert_eq!(key_of(&j).unwrap().label(), "job/care/channel=email");
        j.region = Some("nl".into());
        assert_eq!(key_of(&j).unwrap_err().code(), "invalid_layer_key");
        assert_eq!(
            key_of(&body("tenant")).unwrap_err().code(),
            "invalid_layer_key"
        );
        assert_eq!(
            key_of(&body("profile")).unwrap_err().code(),
            "invalid_layer_key"
        );
        assert_eq!(
            key_of(&body("industry")).unwrap_err().code(),
            "invalid_layer_key"
        );
    }

    #[test]
    fn bad_layer_id_is_404() {
        assert_eq!(layer_id("nope").unwrap_err().code(), "layer_not_found");
        assert!(layer_id(&Uuid::nil().to_string()).is_ok());
    }

    #[test]
    fn layer_view_tag_and_dimension() {
        let at = Utc::now();
        let l = Layer {
            id: Uuid::nil(),
            key: "job/customer_care_agent/channel=email".into(),
            scope: "job".into(),
            industry: None,
            department: None,
            job: Some("customer_care_agent".into()),
            rule: None,
            dimension_key: Some("channel".into()),
            dimension_value: Some("email".into()),
            region: None,
            family: "job/customer_care_agent".into(),
            label: None,
            rollout_wave: None,
            wave_label: None,
            extends: None,
            areas: vec![],
            template_selection: None,
            created_at: at,
            retired_at: None,
        };
        let mut tags = LatestTags::new();
        let v = json!(LayerView::of(&l, &tags));
        assert_eq!(v["tag"], Value::Null);
        assert!(v.get("tagged_at").is_none());
        assert!(v.get("industry").is_none());
        assert_eq!(v["dimension"], json!(["channel", "email"]));
        tags.insert("job/customer_care_agent".into(), (3, at));
        let v = json!(LayerView::of(&l, &tags));
        assert_eq!(v["tag"], "job/customer_care_agent@v3");
        assert!(v.get("tagged_at").is_some());
    }
}
