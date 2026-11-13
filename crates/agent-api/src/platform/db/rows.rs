//! Layer rows (`layer_row`, migration 0041): the revision write every platform edit goes through, and the row
//! readers (head rows, revisions, one row, override targets, untagged drafts).
use std::collections::HashMap;

use agent_model::canonical;
use agent_model::kinds::{self, FieldError, Kind, Op};
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

use super::layers::{self, Layer};
use super::{lock_family, unprocessable};
use crate::audit::{self, Actor, PlatformEvent};
use crate::error::{ApiError, ApiResult};

/// One `layer_row` revision.
#[derive(Debug, Clone, sqlx::FromRow, Serialize)]
pub struct Row {
    pub id: Uuid,
    pub layer_id: Uuid,
    pub kind: String,
    pub key: String,
    pub op: String,
    pub fields: Value,
    pub rev: i32,
    pub is_current: bool,
    pub author: String,
    pub created_at: DateTime<Utc>,
    pub fields_hash: String,
}

/// The columns of [`Row`], in order.
pub(crate) const ROW_COLUMNS: &str =
    "id, layer_id, kind, key, op, fields, rev, is_current, author, created_at, fields_hash";

/// A revision with its layer (the `layer_head` view's shape): head rows of a family, tag members.
#[derive(Debug, Clone, sqlx::FromRow, Serialize)]
pub struct KeyedRow {
    pub id: Uuid,
    pub layer_id: Uuid,
    pub layer_key: String,
    pub family: String,
    pub scope: String,
    pub kind: String,
    pub key: String,
    pub op: String,
    pub fields: Value,
    pub fields_hash: String,
    pub rev: i32,
    pub author: String,
    pub created_at: DateTime<Utc>,
}

/// An untagged current row (B6-5) with its derived draft op (P6 `DraftRevision.op`): `tombstone` when the row
/// is a tombstone, `add` when its (layer, kind, key) is absent from the family's latest tag, else `patch`.
#[derive(Debug, Clone, sqlx::FromRow, Serialize)]
pub struct DraftRow {
    pub id: Uuid,
    pub layer_id: Uuid,
    pub layer_key: String,
    pub family: String,
    pub scope: String,
    pub kind: String,
    pub key: String,
    pub op: String,
    pub fields: Value,
    pub fields_hash: String,
    pub rev: i32,
    pub author: String,
    pub created_at: DateTime<Utc>,
    pub draft_op: String,
}

fn errors_json(errors: &[FieldError]) -> Value {
    serde_json::to_value(errors).unwrap_or(Value::Null)
}

/// Write one revision of (`layer_key`, `kind`, `key`) in the caller's `agent_platform` transaction:
/// family lock, validation (404 `layer_not_found`; 422 `kind_not_allowed_in_layer`, `invalid_key`, `invalid_row`
/// with `errors[]`, `pack_kind_undeclared` for a `knowledge_pack` naming an undeclared kind, D107), retire the current revision, insert `rev + 1` (1 for a new key) with its `fields_hash`, and
/// one `layer.revision` platform audit row. On an `add` or a catalogue `patch` (a whole item) `summary` is
/// recomputed with `kinds::summary`; an overlay `patch` is partial, so its `summary` is kept as given, and a
/// tombstone carries none.
#[allow(clippy::too_many_arguments)]
pub async fn write_revision(
    conn: &mut PgConnection,
    actor: &Actor,
    layer_key: &str,
    kind: Kind,
    key: &str,
    op: Op,
    mut fields: Value,
    author: &str,
) -> ApiResult<Row> {
    let layer = layers::layer_by_key(&mut *conn, layer_key).await?;
    let scope = layer.parsed_scope()?;
    if !kind.allowed_in(scope) {
        let allowed: Vec<&'static str> = kind.scopes().iter().map(|s| s.as_str()).collect();
        return Err(ApiError::Unprocessable(
            "kind_not_allowed_in_layer",
            format!("A {kind} row cannot live in the {scope} layer {layer_key}."),
        )
        .with("allowed_scopes", allowed));
    }
    // D111: platform-wide definitions never live in a region sub-layer (0139 `layer_row_region_kind`)
    if layer.region.is_some() && !kind.allowed_in_region() {
        return Err(ApiError::Unprocessable(
            "kind_not_allowed_in_layer",
            format!("A {kind} row cannot live in the region sub-layer {layer_key}."),
        ));
    }
    if let Err(e) = kinds::validate_key(kind, key) {
        let message = e.message.clone();
        return Err(unprocessable(
            "invalid_key",
            message,
            errors_json(std::slice::from_ref(&e)),
        ));
    }
    if let Err(errors) = kinds::validate_row(kind, op, &fields) {
        let message = match errors.first() {
            Some(first) => format!("The {kind} row {key} is not valid: {first}"),
            None => format!("The {kind} row {key} is not valid."),
        };
        return Err(unprocessable("invalid_row", message, errors_json(&errors)));
    }
    // every writer of an operation row (generic layer POST, generator accept, ...) meets the contract's identity floor
    if kind == Kind::Operation
        && op != Op::Tombstone
        && let Some(level) = fields
            .get("attrs")
            .and_then(|a| a.get("identity_level"))
            .and_then(|l| {
                serde_json::from_value::<agent_model::kinds::catalog::IdentityLevel>(l.clone()).ok()
            })
    {
        crate::runtime::catalog::require_contract_floor(key, level)?;
    }
    // ... and its declared fields: the runtime keeps only declared outputs, so a contract operation never stores them
    // empty (filled from the contract) or outside it (422 `field_not_in_contract`). A `*` add with none declared is
    // filled too; a variant or patch without them inherits.
    if kind == Kind::Operation && op != Op::Tombstone {
        let names = |attrs: &Value, f: &str| -> Option<Vec<String>> {
            attrs.get(f).and_then(Value::as_array).map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect()
            })
        };
        let attrs = fields.get("attrs").cloned().unwrap_or(Value::Null);
        let (i, o) = (
            names(&attrs, "input_fields"),
            names(&attrs, "output_fields"),
        );
        let whole_star = op == Op::Add && !key.contains('{');
        if (i.is_some() || o.is_some() || whole_star)
            && crate::runtime::catalog::contract_def(key).is_some()
        {
            let (mut input, mut output) =
                (i.clone().unwrap_or_default(), o.clone().unwrap_or_default());
            crate::runtime::catalog::fill_contract_io(key, &mut input, &mut output)?;
            if let Some(map) = fields.as_object_mut() {
                let a = map.entry("attrs").or_insert_with(|| serde_json::json!({}));
                if let Some(a) = a.as_object_mut() {
                    if i.is_some() || whole_star {
                        a.insert("input_fields".into(), serde_json::json!(input));
                    }
                    if o.is_some() || whole_star {
                        a.insert("output_fields".into(), serde_json::json!(output));
                    }
                }
            }
        }
    }
    // a procedure's steps are read by the runtime as `ProcedureStep`s; one that does not decode is dropped there
    if kind == Kind::Procedure
        && op != Op::Tombstone
        && let Err(detail) = crate::job_design::types::procedure_steps_decode(&fields)
    {
        let message = format!("The {kind} row {key} is not valid: {detail}");
        let e = FieldError::new("invalid_value", Some("steps"), detail);
        return Err(unprocessable(
            "invalid_row",
            message,
            errors_json(std::slice::from_ref(&e)),
        ));
    }
    // D107 (KH T3): a pack reference names a kind its layer, an ancestor or an extended archetype declares
    if kind == Kind::KnowledgePack && op != Op::Tombstone {
        crate::knowledge::packs::template::check_declared(&mut *conn, &layer, key).await?;
    }
    let mut fields = fields;
    let recompute = match op {
        Op::Add => true,
        Op::Patch => !kind.is_overlay(),
        Op::Tombstone => false,
    };
    if recompute {
        let summary = kinds::summary(kind, &fields);
        if let Some(map) = fields.as_object_mut() {
            if summary.is_empty() {
                map.remove("summary");
            } else {
                map.insert("summary".to_string(), Value::String(summary));
            }
        }
    }

    lock_family(&mut *conn, &layer.family).await?;
    let current: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM layer_row \
          WHERE layer_id = $1 AND kind = $2 AND key = $3 AND is_current \
            FOR UPDATE",
    )
    .bind(layer.id)
    .bind(kind.as_str())
    .bind(key)
    .fetch_optional(&mut *conn)
    .await?;
    let last: Option<i32> = sqlx::query_scalar(
        "SELECT max(rev) FROM layer_row WHERE layer_id = $1 AND kind = $2 AND key = $3",
    )
    .bind(layer.id)
    .bind(kind.as_str())
    .bind(key)
    .fetch_one(&mut *conn)
    .await?;
    let rev = last.unwrap_or(0) + 1;
    if let Some(id) = current {
        sqlx::query("UPDATE layer_row SET is_current = false WHERE id = $1")
            .bind(id)
            .execute(&mut *conn)
            .await?;
    }
    let fields_hash = canonical::fields_hash(&fields);
    let row = sqlx::query_as::<_, Row>(sqlx::AssertSqlSafe(format!(
        "INSERT INTO layer_row (layer_id, kind, key, op, fields, rev, is_current, author, fields_hash) \
         VALUES ($1, $2, $3, $4, $5, $6, true, $7, $8) RETURNING {ROW_COLUMNS}"
    )))
    .bind(layer.id)
    .bind(kind.as_str())
    .bind(key)
    .bind(op.as_str())
    .bind(&fields)
    .bind(rev)
    .bind(author)
    .bind(&fields_hash)
    .fetch_one(&mut *conn)
    .await?;
    audit::record_platform(
        &mut *conn,
        actor,
        PlatformEvent::new("layer.revision", layer_key)
            .detail("family", layer.family.as_str())
            .detail("layer", layer_key)
            .detail("kind", kind.as_str())
            .detail("key", key)
            .detail("op", op.as_str())
            .detail("rev", rev),
    )
    .await?;
    Ok(row)
}

/// The rows of a layer by (kind, key, rev): current only, or with the retired revisions; optionally one kind.
/// 404 `layer_not_found`.
pub async fn layer_rows(
    conn: &mut PgConnection,
    layer_id: Uuid,
    kind: Option<Kind>,
    include_retired: bool,
) -> ApiResult<Vec<Row>> {
    layers::layer_by_id(&mut *conn, layer_id).await?;
    Ok(sqlx::query_as::<_, Row>(sqlx::AssertSqlSafe(format!(
        "SELECT {ROW_COLUMNS} FROM layer_row \
          WHERE layer_id = $1 AND ($2::text IS NULL OR kind = $2) AND (is_current OR $3) \
          ORDER BY kind, key, rev"
    )))
    .bind(layer_id)
    .bind(kind.map(Kind::as_str))
    .bind(include_retired)
    .fetch_all(&mut *conn)
    .await?)
}

/// The current rows of a layer (optionally one kind). 404 `layer_not_found`.
pub async fn head_rows(
    conn: &mut PgConnection,
    layer_id: Uuid,
    kind: Option<Kind>,
) -> ApiResult<Vec<Row>> {
    layer_rows(conn, layer_id, kind, false).await
}

/// The current rows of every layer of a family, by (layer key, kind, key).
pub async fn family_head_rows(conn: &mut PgConnection, family: &str) -> ApiResult<Vec<KeyedRow>> {
    Ok(sqlx::query_as::<_, KeyedRow>(
        "SELECT id, layer_id, layer_key, family, scope, kind, key, op, fields, fields_hash, rev, author, \
                created_at \
           FROM layer_head WHERE family = $1 ORDER BY layer_key, kind, key",
    )
    .bind(family)
    .fetch_all(conn)
    .await?)
}

/// Every revision of (layer, kind, key), oldest first (empty when the key was never written). 404
/// `layer_not_found`.
pub async fn revisions(
    conn: &mut PgConnection,
    layer_id: Uuid,
    kind: Kind,
    key: &str,
) -> ApiResult<Vec<Row>> {
    layers::layer_by_id(&mut *conn, layer_id).await?;
    Ok(sqlx::query_as::<_, Row>(sqlx::AssertSqlSafe(format!(
        "SELECT {ROW_COLUMNS} FROM layer_row WHERE layer_id = $1 AND kind = $2 AND key = $3 ORDER BY rev"
    )))
    .bind(layer_id)
    .bind(kind.as_str())
    .bind(key)
    .fetch_all(&mut *conn)
    .await?)
}

/// One revision by id; 404 `row_not_found`.
pub async fn row_by_id(conn: &mut PgConnection, id: Uuid) -> ApiResult<Row> {
    sqlx::query_as::<_, Row>(sqlx::AssertSqlSafe(format!(
        "SELECT {ROW_COLUMNS} FROM layer_row WHERE id = $1"
    )))
    .bind(id)
    .fetch_optional(conn)
    .await?
    .ok_or_else(|| ApiError::Missing("row_not_found", "No layer row with that id.".into()))
}

/// The untagged current rows of a family (B6-5, the `layer_untagged` view) with their draft op, by (layer key,
/// kind, key). The draft op is computed in SQL here and must equal `agent_model::tags::draft_op` (Ruling 16: the
/// one definition; tombstone first, then `add` when the (layer, kind, key) is absent from the latest tag, else
/// `patch`); the agent-api test `sql_draft_op_equals_agent_model_draft_op` pins the two together.
pub async fn untagged_rows(conn: &mut PgConnection, family: &str) -> ApiResult<Vec<DraftRow>> {
    Ok(sqlx::query_as::<_, DraftRow>(
        "SELECT u.id, u.layer_id, u.layer_key, u.family, u.scope, u.kind, u.key, u.op, u.fields, u.fields_hash, \
                u.rev, u.author, u.created_at, \
                CASE WHEN u.op = 'tombstone' THEN 'tombstone' \
                     WHEN EXISTS ( \
                          SELECT 1 FROM layer_tag_row m JOIN layer_row t ON t.id = m.row_id \
                           WHERE m.family = u.family \
                             AND m.version = (SELECT max(g.version) FROM layer_tag g \
                                               WHERE g.family = u.family AND NOT g.provisional) \
                             AND t.layer_id = u.layer_id AND t.kind = u.kind AND t.key = u.key) \
                     THEN 'patch' \
                     ELSE 'add' END AS draft_op \
           FROM layer_untagged u \
          WHERE u.family = $1 \
          ORDER BY u.layer_key, u.kind, u.key",
    )
    .bind(family)
    .fetch_all(conn)
    .await?)
}

/// What a `patch` / `tombstone` revision acts on (web `LayerOverride`): the nearest earlier layer whose current
/// row `add`s the same (kind, key), or none.
#[derive(Debug, Clone, Serialize)]
pub struct OverrideTarget {
    pub row_id: Uuid,
    pub target: Option<Layer>,
    pub target_row_id: Option<Uuid>,
}

/// D116: `l` is the base_dept layer (or one of its region sub-layers) of the archetype `own` extends, the nearest
/// of the industry-independent base_dept layers for an industry_dept.
fn is_own_archetype(l: &Layer, own: &Layer) -> bool {
    l.scope == "base_dept" && own.extends.is_some() && l.department == own.extends
}

/// The override targets of every non-`add` revision of a layer (retired ones included, as the fixture), by
/// (kind, key, rev). Port of the fixture `layerOverrideTargets`: an earlier layer has a lower scope rank (an
/// un-dimensioned job layer is earlier than a job×dimension layer, a region sub-layer's parent is earlier than the
/// sub-layer); nearest rank first, then the same industry first, then a region sub-layer before its parent, then
/// (where the fixture relied on its array order) by layer key. D116: a base_dept layer is industry-independent like
/// base (an industry_dept prefers the base_dept of the archetype it extends; a base_dept's only earlier layers carry
/// no industry). D111: a region sub-layer only applies with its region,
/// so another region's sub-layers are never earlier, and a layer without a region never targets a sub-layer.
/// 404 `layer_not_found`.
pub async fn override_targets(
    conn: &mut PgConnection,
    layer_id: Uuid,
) -> ApiResult<Vec<OverrideTarget>> {
    let own = layers::layer_by_id(&mut *conn, layer_id).await?;
    let own_rank = own.parsed_scope()?.rank();
    let mut earlier: Vec<(u8, Layer)> = Vec::new();
    for l in layers::layers(&mut *conn).await? {
        if l.id == own.id || (l.region.is_some() && l.region != own.region) {
            continue;
        }
        // D116: a base_dept layer composes on base only (it is industry-independent), so no industry or
        // industry_dept layer is ever earlier than it.
        if own.scope == "base_dept" && l.industry.is_some() {
            continue;
        }
        let rank = l.parsed_scope()?.rank();
        let below = rank < own_rank
            || (rank == own_rank
                && l.scope == "job"
                && l.dimension_key.is_none()
                && own.dimension_key.is_some())
            || (rank == own_rank && own.region.is_some() && l.key == own.family);
        if below {
            earlier.push((rank, l));
        }
    }
    earlier.sort_by(|(ra, a), (rb, b)| {
        rb.cmp(ra)
            .then_with(|| (b.industry == own.industry).cmp(&(a.industry == own.industry)))
            .then_with(|| is_own_archetype(b, &own).cmp(&is_own_archetype(a, &own)))
            .then_with(|| b.region.is_some().cmp(&a.region.is_some()))
            .then_with(|| a.key.cmp(&b.key))
    });

    let rows: Vec<(Uuid, String, String)> = sqlx::query_as(
        "SELECT id, kind, key FROM layer_row WHERE layer_id = $1 AND op <> 'add' ORDER BY kind, key, rev",
    )
    .bind(layer_id)
    .fetch_all(&mut *conn)
    .await?;
    let hits: Vec<(Uuid, Uuid, String, String)> = sqlx::query_as(
        "SELECT h.id, h.layer_id, h.kind, h.key FROM layer_row h \
          WHERE h.is_current AND h.op = 'add' AND h.layer_id <> $1 \
            AND EXISTS (SELECT 1 FROM layer_row o \
                         WHERE o.layer_id = $1 AND o.op <> 'add' AND o.kind = h.kind AND o.key = h.key)",
    )
    .bind(layer_id)
    .fetch_all(&mut *conn)
    .await?;
    // (kind, key) → [(layer id, row id)] of the current adds elsewhere.
    let mut adds: HashMap<(String, String), Vec<(Uuid, Uuid)>> = HashMap::new();
    for (row_id, layer, kind, key) in hits {
        adds.entry((kind, key)).or_default().push((layer, row_id));
    }
    Ok(rows
        .into_iter()
        .map(|(row_id, kind, key)| {
            let found = adds.get(&(kind, key)).and_then(|candidates| {
                earlier.iter().find_map(|(_, l)| {
                    candidates
                        .iter()
                        .find(|(lid, _)| *lid == l.id)
                        .map(|(_, rid)| (l.clone(), *rid))
                })
            });
            match found {
                Some((target, target_row_id)) => OverrideTarget {
                    row_id,
                    target: Some(target),
                    target_row_id: Some(target_row_id),
                },
                None => OverrideTarget {
                    row_id,
                    target: None,
                    target_row_id: None,
                },
            }
        })
        .collect())
}
