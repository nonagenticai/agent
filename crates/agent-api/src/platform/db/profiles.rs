//! The business profiles (`profile` rows, P12) as the compile and the selection options read them.
use std::collections::BTreeMap;

use agent_model::kinds::profile::ProfileFields;
use sqlx::PgConnection;

use super::{corrupt, tags::StackEntry};
use crate::error::ApiResult;

/// One published, not tombstoned `profile` row: its key, the layer holding it and its fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileEntry {
    pub key: String,
    pub layer_key: String,
    pub fields: ProfileFields,
}

impl ProfileEntry {
    /// The profile's values as a release freezes them: axis → its selected values joined with ` + `, axes with no
    /// value left out.
    pub fn values(&self) -> BTreeMap<String, String> {
        self.fields
            .selection
            .iter()
            .filter(|(_, v)| !v.is_empty())
            .map(|(axis, v)| (axis.clone(), v.join(" + ")))
            .collect()
    }
}

/// Every published `profile` row, by (key, layer): the rows in each family's newest non-provisional tag
/// (a tombstone there removes it). A drafted, untagged profile is not offered, the rule industries and departments
/// follow (`release::compile::db_options`): the compile reads tagged rows only, so offering a draft would let a
/// tenant pick a profile its release cannot find. A row that does not read as a profile is a database corruption
/// error.
pub async fn profiles(conn: &mut PgConnection) -> ApiResult<Vec<ProfileEntry>> {
    let rows: Vec<(String, String, serde_json::Value)> = sqlx::query_as(
        "SELECT r.key, l.key, r.fields FROM layer_tag_row tr \
           JOIN (SELECT family, max(version) AS version FROM layer_tag \
                  WHERE NOT provisional GROUP BY family) t \
             ON t.family = tr.family AND t.version = tr.version \
           JOIN layer_row r ON r.id = tr.row_id \
           JOIN layer l ON l.id = r.layer_id \
          WHERE r.kind = 'profile' AND r.op <> 'tombstone' ORDER BY r.key, l.key",
    )
    .fetch_all(conn)
    .await?;
    rows.into_iter().map(entry).collect()
}

/// The profile in the release's exact pins, independent of newer published versions and drafts.
/// As in `ProfileDef::from_rows`, the last layer in byte order wins when two pinned layers define the key.
pub async fn pinned_profile(
    conn: &mut PgConnection,
    key: &str,
    pins: &[StackEntry],
) -> ApiResult<Option<ProfileEntry>> {
    let families: Vec<_> = pins.iter().map(|p| p.layer.clone()).collect();
    let versions: Vec<_> = pins.iter().map(|p| p.version).collect();
    let row: Option<(String, String, serde_json::Value)> = sqlx::query_as(
        "SELECT r.key, l.key, r.fields FROM unnest($2::text[], $3::integer[]) p(family, version) \
           JOIN layer_tag t ON t.family = p.family AND t.version = p.version AND NOT t.provisional \
           JOIN layer_tag_row tr ON tr.family = t.family AND tr.version = t.version \
           JOIN layer_row r ON r.id = tr.row_id \
           JOIN layer l ON l.id = r.layer_id \
          WHERE r.kind = 'profile' AND r.key = $1 AND r.op <> 'tombstone' \
          ORDER BY l.key COLLATE \"C\" DESC LIMIT 1",
    )
    .bind(key)
    .bind(&families)
    .bind(&versions)
    .fetch_optional(conn)
    .await?;
    row.map(entry).transpose()
}

fn entry((key, layer_key, fields): (String, String, serde_json::Value)) -> ApiResult<ProfileEntry> {
    let fields = serde_json::from_value(fields).map_err(|e| {
        corrupt(format!(
            "profile {key} (layer {layer_key}) is unreadable: {e}"
        ))
    })?;
    Ok(ProfileEntry {
        key,
        layer_key,
        fields,
    })
}
