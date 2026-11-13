//! B6b platform-model area `export` (plan B6 §5, T12f F-30; the KH bucket of Revision 2026-10-22).
//!
//! `GET /api/platform/export[?family=&tag=v{n}]` (exportPlatformSeed) → an `application/json` attachment
//! `agent-platform-seed-<ts>.json` in the seed format the boot seeder reads (crate::platform::seeder, one full
//! version: `layers`, `rows` with their full history, `tags` with their member row ids, `vocab` with every value,
//! `job_templates`, `report: []`), so importing it into an empty database with `seeder::apply` reproduces every
//! tag's `content_hash` (test `export_round_trips_through_seeder`).
//!
//! - `?family=<key or dotted slug>` limits layers, rows, tags, axis rules and job templates to one template family;
//!   404 `layer_not_found`. `&tag=v{n}` limits rows to that tag's members and tags to that tag; 404
//!   `tag_not_found`; a `tag` without a `family` is 400 `invalid_query`.
//! - One `REPEATABLE READ READ ONLY` snapshot for the read, then one `platform.export` platform audit row in its
//!   own `agent_platform` transaction (the read itself never writes).
//! - Two fields go beyond what the seed files carry, and are written only when set: a region sub-layer's `region`
//!   (D111) and a retired axis value's `retired_at` / `retired_by` (0160). The seeder reads both (optional fields of
//!   `SeedLayer` / `SeedAxisValue`), so they round-trip (test `export_round_trips_region_layer_and_retired_value`).
use axum::extract::State;
use axum::http::header::{CONTENT_DISPOSITION, CONTENT_TYPE};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use chrono::Utc;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgConnection;

use super::templates::parse_tag;
use super::{Who, audit, layer_from_slug, missing, read_tx, write_tx};
use crate::audit::PlatformEvent;
use crate::error::{ApiError, ApiQuery, ApiResult};
use crate::platform::db::{layers, tags};
use crate::platform::templates::job_templates;
use crate::state::AppState;
use crate::tenancy::SuperAdmin;

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/platform/export", get(export_platform_seed))
}

#[derive(Debug, Default, Deserialize)]
pub struct ExportQuery {
    pub family: Option<String>,
    pub tag: Option<String>,
}

/// What an export covers: everything, one family, or one tag of one family.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    pub family: Option<String>,
    pub tag: Option<i32>,
}

const LAYERS: &str = "SELECT jsonb_build_object( \
        'id', l.id, 'key', l.key, 'scope', l.scope, 'industry', l.industry, 'department', l.department, \
        'job', l.job, 'rule', l.rule, 'dimension_key', l.dimension_key, 'dimension_value', l.dimension_value, \
        'family', l.family, 'label', l.label, 'rollout_wave', l.rollout_wave, 'wave_label', l.wave_label, \
        'extends', l.extends, 'areas', to_jsonb(l.areas), 'template_selection', l.template_selection, \
        'created_at', l.created_at) \
     || CASE WHEN l.region IS NULL THEN '{}'::jsonb ELSE jsonb_build_object('region', l.region) END \
   FROM layer l \
  WHERE ($1::text IS NULL OR l.family = $1) \
  ORDER BY (l.family = l.key) DESC, l.key";

const ROWS: &str = "SELECT jsonb_build_object( \
        'id', r.id, 'layer_key', l.key, 'kind', r.kind, 'key', r.key, 'op', r.op, 'rev', r.rev, \
        'is_current', r.is_current, 'fields', r.fields, 'author', r.author, 'created_at', r.created_at) \
   FROM layer_row r JOIN layer l ON l.id = r.layer_id \
  WHERE ($1::text IS NULL OR l.family = $1) \
    AND ($2::int IS NULL OR EXISTS (SELECT 1 FROM layer_tag_row m \
                                     WHERE m.family = $1 AND m.version = $2 AND m.row_id = r.id)) \
  ORDER BY l.key, r.kind, r.key, r.rev";

const TAGS: &str = "SELECT jsonb_build_object( \
        'family', t.family, 'version', t.version, 'note', t.note, 'provisional', t.provisional, \
        'tagged_by', t.tagged_by, 'tagged_at', t.tagged_at, 'stack', t.stack, \
        'members', coalesce((SELECT jsonb_agg(m.row_id ORDER BY m.row_id) FROM layer_tag_row m \
                              WHERE m.family = t.family AND m.version = t.version), '[]'::jsonb)) \
   FROM layer_tag t \
  WHERE ($1::text IS NULL OR t.family = $1) AND ($2::int IS NULL OR t.version = $2) \
  ORDER BY t.tagged_at, t.family, t.version";

const AXES: &str = "SELECT jsonb_build_object( \
        'key', a.key, 'label', a.label, 'description', a.description, 'cardinality', a.cardinality, \
        'object', a.object, 'milestone', a.milestone, 'created_at', a.created_at, \
        'values', coalesce((SELECT jsonb_agg( \
                jsonb_build_object('key', v.key, 'label', v.label, 'parent', v.parent, 'added_at', v.added_at, \
                                   'added_by', v.added_by) \
                || CASE WHEN v.retired_at IS NULL THEN '{}'::jsonb \
                        ELSE jsonb_build_object('retired_at', v.retired_at, 'retired_by', v.retired_by) END \
                ORDER BY v.key) \
            FROM axis_value v WHERE v.axis = a.key), '[]'::jsonb)) \
   FROM axis a ORDER BY a.key";

const AXIS_RULES: &str = "SELECT jsonb_build_object( \
        'key', r.key, 'priority', r.priority, 'condition', r.condition, 'layer_key', l.key, 'owner', r.owner, \
        'created_by', r.created_by, 'created_at', r.created_at) \
   FROM axis_rule r JOIN layer l ON l.id = r.layer_id \
  WHERE ($1::text IS NULL OR l.family = $1) \
  ORDER BY r.key";

const DIMENSIONS: &str = "SELECT jsonb_build_object( \
        'key', d.key, 'label', d.label, 'description', d.description, 'kind', d.kind, 'concerns', d.concerns, \
        'values', coalesce((SELECT jsonb_agg(jsonb_build_object('key', v.key, 'label', v.label, 'note', v.note, \
                                                                'added_at', v.added_at, 'added_by', v.added_by) \
                                             ORDER BY v.key) \
                              FROM dimension_value v WHERE v.dimension = d.key), '[]'::jsonb)) \
   FROM dimension d ORDER BY d.key";

const RENEWAL_PRESETS: &str = "SELECT jsonb_build_object( \
        'key', p.key, 'label', p.label, 'example', p.example, 'order_type', p.order_type, \
        'initiator', p.initiator, 'mode', p.mode) \
   FROM renewal_preset p ORDER BY p.key";

const FOLDED_CONCEPTS: &str = "SELECT jsonb_build_object( \
        'concept', f.concept, 'becomes', f.becomes, 'object', f.object, 'cite', f.cite) \
   FROM folded_concept f ORDER BY f.concept";

const COMPILE_REFERENCES: &str = "SELECT jsonb_build_object( \
        'key', c.key, 'label', c.label, 'note', c.note, 'selection', c.selection, 'job', c.job) \
   FROM compile_reference_seed c ORDER BY c.key";

async fn list(conn: &mut PgConnection, sql: &'static str) -> ApiResult<Vec<Value>> {
    Ok(sqlx::query_scalar::<_, Value>(sql).fetch_all(conn).await?)
}

async fn list_for(conn: &mut PgConnection, sql: &'static str, s: &Scope) -> ApiResult<Vec<Value>> {
    Ok(sqlx::query_scalar::<_, Value>(sql)
        .bind(s.family.as_deref())
        .bind(s.tag)
        .fetch_all(conn)
        .await?)
}

/// The export scope of a query: 404 `layer_not_found` (no such template family), `tag_not_found`; 400
/// `invalid_query` for a tag without a family.
pub async fn scope_of(conn: &mut PgConnection, q: &ExportQuery) -> ApiResult<Scope> {
    let family = match q.family.as_deref().filter(|f| !f.is_empty()) {
        None => None,
        Some(raw) => {
            let key = layer_from_slug(raw);
            let head = layers::find_layer_by_key(&mut *conn, &key).await?;
            match head {
                Some(h) if h.is_head() && h.scope != "profile" => Some(key),
                _ => {
                    return Err(missing(
                        "layer_not_found",
                        format!("No template family “{key}”."),
                    ));
                }
            }
        }
    };
    let tag = match (q.tag.as_deref().filter(|t| !t.is_empty()), &family) {
        (None, _) => None,
        (Some(_), None) => {
            return Err(ApiError::BadRequest(
                "invalid_query",
                "tag needs a family (?family=<layer>&tag=v<n>).".into(),
            ));
        }
        (Some(raw), Some(f)) => {
            let v = parse_tag(f, raw)?;
            tags::tag(&mut *conn, f, v).await?;
            Some(v)
        }
    };
    Ok(Scope { family, tag })
}

/// The seed document of `s`, read in the caller's transaction.
pub async fn export(conn: &mut PgConnection, s: &Scope) -> ApiResult<Value> {
    let layers = sqlx::query_scalar::<_, Value>(LAYERS)
        .bind(s.family.as_deref())
        .fetch_all(&mut *conn)
        .await?;
    let rows = list_for(&mut *conn, ROWS, s).await?;
    let tags = list_for(&mut *conn, TAGS, s).await?;
    let axis_rules = sqlx::query_scalar::<_, Value>(AXIS_RULES)
        .bind(s.family.as_deref())
        .fetch_all(&mut *conn)
        .await?;
    let vocab = json!({
        "axes": list(&mut *conn, AXES).await?,
        "axis_rules": axis_rules,
        "dimensions": list(&mut *conn, DIMENSIONS).await?,
        "renewal_presets": list(&mut *conn, RENEWAL_PRESETS).await?,
        "folded_concepts": list(&mut *conn, FOLDED_CONCEPTS).await?,
        "compile_reference_seeds": list(&mut *conn, COMPILE_REFERENCES).await?,
    });
    let templates: Vec<Value> = job_templates(&mut *conn)
        .await?
        .into_iter()
        .filter(|t| s.family.as_deref().is_none_or(|f| t.layer == f))
        .filter(|t| s.tag.is_none_or(|v| t.version == v))
        .map(|t| json!(t))
        .collect();
    Ok(json!({
        "format": "agent-platform-seed",
        "exported_at": Utc::now(),
        "family": s.family,
        "tag": s.tag,
        "layers": layers,
        "rows": rows,
        "tags": tags,
        "vocab": vocab,
        "job_templates": templates,
        "report": [],
    }))
}

async fn export_platform_seed(
    sa: SuperAdmin,
    State(st): State<AppState>,
    ApiQuery(q): ApiQuery<ExportQuery>,
) -> ApiResult<impl IntoResponse> {
    let who = Who::of(&sa);
    let mut tx = read_tx(&st).await?;
    let scope = scope_of(&mut tx, &q).await?;
    let doc = export(&mut tx, &scope).await?;
    tx.rollback().await?;

    let count = |k: &str| doc[k].as_array().map_or(0, Vec::len);
    let mut w = write_tx(&st).await?;
    audit(
        &mut w,
        &who,
        PlatformEvent::new(
            "platform.export",
            scope.family.as_deref().unwrap_or("platform"),
        )
        .detail("family", scope.family.clone())
        .detail("tag", scope.tag)
        .detail("layers", count("layers"))
        .detail("rows", count("rows"))
        .detail("tags", count("tags")),
    )
    .await?;
    w.commit().await?;

    let file = format!(
        "attachment; filename=\"agent-platform-seed-{}.json\"",
        Utc::now().format("%Y%m%dT%H%M%SZ")
    );
    Ok((
        [
            (CONTENT_TYPE, "application/json".to_string()),
            (CONTENT_DISPOSITION, file),
        ],
        Json(doc),
    ))
}
