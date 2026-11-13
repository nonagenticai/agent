//! The head catalogue of the object model (plan B6 §5 T11/T12c/T12f): the current `object`, `operation` and
//! `error_state` rows read from `layer_head`, one row per key, resolved as the web does (`resolveOperations`).
//!
//! **Which row is the catalogue's.** A key can be carried by several layers (`case` in `base` and patched in
//! `job/it_service_desk_agent`). The platform catalogue is the broadest definition: the current row of the
//! broadest scope (base → industry → industry_dept → job, then layer key); a tombstone there retires the key from the
//! catalogue. Every catalogue edit writes the next revision of exactly that row, in its own layer (T12c "in the
//! row's own layer"); deeper layers stay visible through `objectModelMap.layers` and `objectContext`.
use std::collections::{BTreeMap, BTreeSet};

use agent_model::catalog::{ObjectDef, OperationDefRow, OperationRow, resolve_operations};
use agent_model::kinds::{self, Kind};
use serde_json::Value;
use sqlx::PgConnection;

use crate::error::ApiResult;
use crate::platform::db::corrupt;

/// One current row with its layer (the `layer_head` view joined to `layer` for the label).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct HeadItem {
    pub layer_key: String,
    pub layer_label: Option<String>,
    pub family: String,
    pub scope: String,
    pub kind: String,
    pub key: String,
    pub op: String,
    pub fields: Value,
    pub rev: i32,
}

/// base 0 · industry 1 · base_dept 2 (D116) · industry_dept 3 · job 4 · anything else (profile) last.
pub fn scope_rank(scope: &str) -> u8 {
    match scope {
        "base" => 0,
        "industry" => 1,
        "base_dept" => 2,
        "industry_dept" => 3,
        "job" => 4,
        _ => 5,
    }
}

/// The current rows of `kinds`, by (kind, key, layer key).
pub async fn head_items(conn: &mut PgConnection, kinds: &[Kind]) -> ApiResult<Vec<HeadItem>> {
    let kinds: Vec<String> = kinds.iter().map(|k| k.as_str().to_owned()).collect();
    Ok(sqlx::query_as::<_, HeadItem>(
        "SELECT h.layer_key, l.label AS layer_label, h.family, h.scope, h.kind, h.key, h.op, h.fields, h.rev \
           FROM layer_head h \
           JOIN layer l ON l.id = h.layer_id \
          WHERE h.kind = ANY($1) \
          ORDER BY h.kind, h.key, h.layer_key",
    )
    .bind(&kinds)
    .fetch_all(conn)
    .await?)
}

/// The catalogue row of every key of `kind` (module doc), by key; retired keys left out.
pub fn catalogue_rows(items: &[HeadItem], kind: Kind) -> Vec<&HeadItem> {
    let mut best: BTreeMap<&str, &HeadItem> = BTreeMap::new();
    for it in items.iter().filter(|i| i.kind == kind.as_str()) {
        let better = match best.get(it.key.as_str()) {
            None => true,
            Some(b) => {
                (scope_rank(&it.scope), it.layer_key.as_str())
                    < (scope_rank(&b.scope), b.layer_key.as_str())
            }
        };
        if better {
            best.insert(it.key.as_str(), it);
        }
    }
    best.into_values().filter(|i| i.op != "tombstone").collect()
}

/// The head catalogue: objects and operation rows (each with its catalogue row) and the resolved operations.
#[derive(Debug, Clone)]
pub struct Catalogue {
    pub objects: Vec<(HeadItem, ObjectDef)>,
    pub ops: Vec<(HeadItem, OperationDefRow)>,
    pub resolved: Vec<OperationRow>,
}

impl Catalogue {
    pub fn from_items(items: &[HeadItem]) -> ApiResult<Catalogue> {
        let mut objects = Vec::new();
        for it in catalogue_rows(items, Kind::Object) {
            let def = ObjectDef::from_row(&it.key, &it.fields).map_err(corrupt)?;
            objects.push((it.clone(), def));
        }
        let mut ops = Vec::new();
        for it in catalogue_rows(items, Kind::Operation) {
            let row = OperationDefRow::from_row(&it.key, &it.fields).map_err(corrupt)?;
            ops.push((it.clone(), row));
        }
        let defs: Vec<ObjectDef> = objects.iter().map(|(_, d)| d.clone()).collect();
        let rows: Vec<OperationDefRow> = ops.iter().map(|(_, r)| r.clone()).collect();
        let resolved = resolve_operations(&rows, &defs);
        Ok(Catalogue {
            objects,
            ops,
            resolved,
        })
    }

    pub fn object(&self, key: &str) -> Option<&(HeadItem, ObjectDef)> {
        self.objects.iter().find(|(_, d)| d.object == key)
    }

    /// The operation row stored under `key` (`order.cancel`, `case.create{return}`).
    pub fn op_row(&self, key: &str) -> Option<&(HeadItem, OperationDefRow)> {
        self.ops.iter().find(|(i, _)| i.key == key)
    }

    /// The resolved operation `key`.
    pub fn resolved_op(&self, key: &str) -> Option<&OperationRow> {
        self.resolved.iter().find(|r| r.key == key)
    }

    /// The resolved operations on `object`.
    pub fn ops_of(&self, object: &str) -> Vec<&OperationRow> {
        self.resolved
            .iter()
            .filter(|r| r.object == object)
            .collect()
    }
}

/// The head catalogue of objects and operations.
pub async fn load(conn: &mut PgConnection) -> ApiResult<Catalogue> {
    let items = head_items(conn, &[Kind::Object, Kind::Operation]).await?;
    Catalogue::from_items(&items)
}

/// The (layer key, key) of every untagged current row of `kind` (B6-5, `layer_untagged`).
pub async fn untagged(
    conn: &mut PgConnection,
    kind: Kind,
) -> ApiResult<BTreeSet<(String, String)>> {
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT layer_key, key FROM layer_untagged WHERE kind = $1")
            .bind(kind.as_str())
            .fetch_all(conn)
            .await?;
    Ok(rows.into_iter().collect())
}

/// The first non-provisional tag version that carries (layer key, key) of `kind` (not as a tombstone).
pub async fn first_tags(
    conn: &mut PgConnection,
    kind: Kind,
) -> ApiResult<BTreeMap<(String, String), i32>> {
    let rows: Vec<(String, String, i32)> = sqlx::query_as(
        "SELECT l.key, r.key, min(m.version) \
           FROM layer_tag_row m \
           JOIN layer_tag t ON t.family = m.family AND t.version = m.version \
           JOIN layer_row r ON r.id = m.row_id \
           JOIN layer l ON l.id = r.layer_id \
          WHERE r.kind = $1 AND NOT t.provisional AND r.op <> 'tombstone' \
          GROUP BY l.key, r.key",
    )
    .bind(kind.as_str())
    .fetch_all(conn)
    .await?;
    Ok(rows.into_iter().map(|(l, k, v)| ((l, k), v)).collect())
}

/// The display names of `subs` (`agent_actor_name`, else the sub itself; platform authors have no `users` row).
pub async fn names(
    conn: &mut PgConnection,
    subs: &[String],
) -> ApiResult<BTreeMap<String, String>> {
    let rows: Vec<(String, Option<String>)> =
        sqlx::query_as("SELECT s, agent_actor_name(s) FROM unnest($1::text[]) AS s")
            .bind(subs)
            .fetch_all(conn)
            .await?;
    Ok(rows
        .into_iter()
        .map(|(s, n)| {
            let name = n
                .filter(|n| !n.trim().is_empty())
                .unwrap_or_else(|| s.clone());
            (s, name)
        })
        .collect())
}

/// The one-line detail of a row: its stored `summary`, else the kind's computed one.
pub fn detail(kind: Kind, fields: &Value) -> String {
    match fields.get("summary").and_then(Value::as_str) {
        Some(s) => s.to_owned(),
        None => kinds::summary(kind, fields),
    }
}

/// Does `text` name `object` as the web's `mentions` does (`order.cancel`, `case{return}`, `object delivery`)?
/// Port of `(^|[^a-z_])(o\.[a-z_]|o\{|object o\b)`.
pub fn mentions(text: &str, object: &str) -> bool {
    if object.is_empty() {
        return false;
    }
    let word = |c: char| c.is_ascii_lowercase() || c == '_';
    let boundary = |c: char| !(c.is_ascii_alphanumeric() || c == '_');
    for (i, _) in text.match_indices(object) {
        let head = &text[..i];
        let rest = &text[i + object.len()..];
        let mut after = rest.chars();
        let qualified = match after.next() {
            Some('.') => after.next().is_some_and(word),
            Some('{') => true,
            _ => false,
        };
        if qualified && head.chars().next_back().is_none_or(|c| !word(c)) {
            return true;
        }
        if let Some(pre) = head.strip_suffix("object ")
            && pre.chars().next_back().is_none_or(|c| !word(c))
            && rest.chars().next().is_none_or(boundary)
        {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn item(layer: &str, scope: &str, kind: &str, key: &str, op: &str) -> HeadItem {
        HeadItem {
            layer_key: layer.into(),
            layer_label: None,
            family: layer.into(),
            scope: scope.into(),
            kind: kind.into(),
            key: key.into(),
            op: op.into(),
            fields: json!({}),
            rev: 1,
        }
    }

    #[test]
    fn broadest_row_wins_and_a_tombstone_retires() {
        let items = vec![
            item("job/it", "job", "object", "case", "patch"),
            item("base", "base", "object", "case", "add"),
            item("industry/u", "industry", "object", "location", "add"),
            item("base", "base", "object", "gone", "tombstone"),
            item("job/x", "job", "object", "gone", "add"),
            item("base", "base", "operation", "case.read", "add"),
        ];
        let rows = catalogue_rows(&items, Kind::Object);
        let got: Vec<(&str, &str)> = rows
            .iter()
            .map(|i| (i.key.as_str(), i.layer_key.as_str()))
            .collect();
        assert_eq!(got, vec![("case", "base"), ("location", "industry/u")]);
    }

    #[test]
    fn mentions_like_the_web() {
        assert!(mentions("order.cancel", "order"));
        assert!(mentions("binds case{return}", "case"));
        assert!(mentions("reads object delivery", "delivery"));
        assert!(mentions("object delivery.", "delivery"));
        assert!(!mentions("order_line.read", "order"));
        assert!(!mentions("reorder.x", "order"));
        assert!(!mentions("the order", "order"));
        assert!(!mentions("object deliveryman", "delivery"));
        assert!(!mentions("order.", "order"));
        assert!(!mentions("order.Cancel", "order"));
        assert!(mentions("x order.read y", "order"));
    }

    #[test]
    fn detail_prefers_stored_summary() {
        assert_eq!(
            detail(
                Kind::ErrorState,
                &json!({"summary": "s", "description": "d"})
            ),
            "s"
        );
        assert_eq!(detail(Kind::ErrorState, &json!({"description": "d"})), "d");
    }
}
