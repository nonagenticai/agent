//! Tenant field-mapping overrides (B7 T12; 0094 `field_mapping_override`): a tenant replaces the JSONPath one op of
//! the pinned template reads one canonical field from. The rows are DRAFT working rows (D69, ledger kind
//! `mapping_override`, tenant-wide): the test env runs them, Production reads the job's release snapshot.
//!
//! - [`effective_field_map`] is the pure merge (override path beats the template path, keyed by field).
//! - [`working`] reads the working rows; [`for_env`] is what a job-less call on an instance (a T3 contract test)
//!   applies: `test` → the working rows, anything else → none (prod never reads the draft). A job's Production
//!   call reads the overrides of its current release snapshot instead (crate::connectors::runtime,
//!   crate::connectors::places::live_connectors).
//! - The rows are written only through crate::connectors::places (inside `design::track_tenant`).
//! - [`check_path`] is the save rule of the fixture's `saveMappingOverride` (sample of this op → the path must match
//!   it, else the path must start with `$`).
use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sqlx::PgConnection;

use super::jsonpath::{js_space, json_path};
use super::model::FieldMapEntry;
use crate::error::{ApiError, ApiResult};

/// One `field_mapping_override` row.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct OverrideRow {
    pub instance_id: String,
    pub op: String,
    pub field: String,
    pub default_path: String,
    pub path: String,
    pub updated_by: String,
    pub updated_at: DateTime<Utc>,
}

impl OverrideRow {
    /// `FieldMappingOverride` JSON (`web/src/lib/api/types/connectors.ts`).
    pub fn json(&self) -> Value {
        json!({
            "op": self.op,
            "field": self.field,
            "default_path": self.default_path,
            "path": self.path,
            "updated_by": self.updated_by,
            "updated_at": self.updated_at,
        })
    }
}

/// `field → path` of the rows of `op` (the shape `mapping::extract` takes).
pub fn override_map(rows: &[OverrideRow], op: &str) -> BTreeMap<String, String> {
    rows.iter()
        .filter(|r| r.op == op)
        .map(|r| (r.field.clone(), r.path.clone()))
        .collect()
}

/// The field map an op runs with: every template entry, its path replaced by the override of its field when there
/// is one (transform and order unchanged). An override naming a field the template does not map adds nothing.
pub fn effective_field_map(
    base: &[FieldMapEntry],
    overrides: &BTreeMap<String, String>,
) -> Vec<FieldMapEntry> {
    base.iter()
        .map(|e| match overrides.get(&e.field) {
            Some(path) => FieldMapEntry {
                path: path.clone(),
                ..e.clone()
            },
            None => e.clone(),
        })
        .collect()
}

/// The save rule of an override path: trimmed; checked against the stored sample when the sample is of this op
/// (422 `path_no_match` with the JSONPath error), else it must start with `$` (422 `invalid_path`).
pub fn check_path(path: &str, sample: Option<(&str, &Value)>, op: &str) -> ApiResult<String> {
    let p = path.trim_matches(js_space).to_owned();
    match sample {
        Some((sample_op, body)) if sample_op == op => {
            if let Err(e) = json_path(body, &p) {
                return Err(ApiError::Unprocessable(
                    "path_no_match",
                    format!("{p}: {e}"),
                ));
            }
        }
        _ => {
            if !p.starts_with('$') {
                return Err(ApiError::Unprocessable(
                    "invalid_path",
                    "A JSONPath starts with $.".into(),
                ));
            }
        }
    }
    if p.chars().count() > 512 {
        return Err(ApiError::Unprocessable(
            "invalid_path",
            "A JSONPath is at most 512 characters.".into(),
        ));
    }
    Ok(p)
}

/// Working rows of one instance (`Some`) or of all (`None`), by instance, op, field.
pub async fn working(
    conn: &mut PgConnection,
    instance: Option<&str>,
) -> ApiResult<Vec<OverrideRow>> {
    Ok(sqlx::query_as::<_, OverrideRow>(
        "SELECT instance_id, op, field, default_path, path, updated_by, updated_at FROM field_mapping_override \
          WHERE ($1::text IS NULL OR instance_id = $1) ORDER BY instance_id, op, field",
    )
    .bind(instance)
    .fetch_all(&mut *conn)
    .await?)
}

/// The overrides a call of `op` on an instance of `env` applies (`field → path`).
pub async fn for_env(
    conn: &mut PgConnection,
    instance: &str,
    env: &str,
    op: &str,
) -> ApiResult<BTreeMap<String, String>> {
    if env != "test" {
        // A job-less prod call has no release snapshot to read: the template default, never the working draft row
        // (a job's Production call reads its snapshot: crate::connectors::runtime).
        return Ok(BTreeMap::new());
    }
    let rows = working(conn, Some(instance)).await?;
    Ok(override_map(&rows, op))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(field: &str, path: &str, transform: Option<&str>) -> FieldMapEntry {
        FieldMapEntry {
            path: path.to_owned(),
            field: field.to_owned(),
            transform: transform.map(str::to_owned),
        }
    }

    #[test]
    fn override_beats_template_path_and_keeps_transform_and_order() {
        let base = vec![
            entry("status", "$.status", Some("enum_map")),
            entry("total", "$.total", None),
        ];
        let ov = BTreeMap::from([
            ("status".to_owned(), "$.data.state".to_owned()),
            ("not_mapped".to_owned(), "$.x".to_owned()),
        ]);
        let got = effective_field_map(&base, &ov);
        assert_eq!(
            got,
            vec![
                entry("status", "$.data.state", Some("enum_map")),
                entry("total", "$.total", None)
            ]
        );
        assert_eq!(effective_field_map(&base, &BTreeMap::new()), base);
    }

    #[test]
    fn override_map_is_per_op() {
        let row = |op: &str, field: &str, path: &str| OverrideRow {
            instance_id: "shop".into(),
            op: op.into(),
            field: field.into(),
            default_path: "$.x".into(),
            path: path.into(),
            updated_by: "u".into(),
            updated_at: Utc::now(),
        };
        let rows = vec![
            row("order.read", "status", "$.a"),
            row("order.list", "status", "$.b"),
        ];
        assert_eq!(
            override_map(&rows, "order.read"),
            BTreeMap::from([("status".to_owned(), "$.a".to_owned())])
        );
        assert!(override_map(&rows, "case.read").is_empty());
    }

    #[test]
    fn path_rules() {
        let body = json!({"status": "open"});
        assert_eq!(
            check_path(" $.status ", Some(("order.read", &body)), "order.read").unwrap(),
            "$.status"
        );
        let e = check_path("$.nope", Some(("order.read", &body)), "order.read").unwrap_err();
        assert!(matches!(e, ApiError::Unprocessable("path_no_match", _)));
        // a sample of another op is not checked against; the path must still start with $
        assert!(check_path("$.nope", Some(("order.list", &body)), "order.read").is_ok());
        let e = check_path("status", None, "order.read").unwrap_err();
        assert!(matches!(e, ApiError::Unprocessable("invalid_path", _)));
    }
}
