//! Family tags (`layer_tag` + `layer_tag_row`, migration 0041; plan B6-3, B6-4, B6-5): tagging a family, its
//! stack, the content hash, and the tag readers. `tag_family` also runs the `job_template_tag` projection
//! (crate::platform::templates, Ruling 17) under the family lock.
use std::collections::BTreeMap;

use agent_model::layer::Scope;
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

use super::layers::{self, LAYER_COLUMNS, Layer};
use super::lock_family;
use super::rows::KeyedRow;
use crate::audit::{self, Actor, PlatformEvent};
use crate::error::{ApiError, ApiResult};
use crate::platform::hooks::GeneratorGuard;

/// `layer_tag.note` is at most this many characters (the table's CHECK).
pub const MAX_NOTE_CHARS: usize = 2000;

/// One `layer_tag` row. `stack` is `[{layer, version}]` ([`StackEntry`]).
#[derive(Debug, Clone, sqlx::FromRow, Serialize)]
pub struct TemplateTag {
    pub family: String,
    pub version: i32,
    pub content_hash: String,
    pub note: String,
    pub provisional: bool,
    pub stack: Value,
    pub tagged_by: String,
    pub tagged_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
}

const TAG_COLUMNS: &str =
    "family, version, content_hash, note, provisional, stack, tagged_by, tagged_at, created_at";

/// One family a tag was cut on top of: its latest non-provisional version at tag time.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct StackEntry {
    pub layer: String,
    pub version: i32,
}

/// The tag content hash and its member type live in `agent_model::tags` (Ruling 16: one definition); re-exported
/// here under the names the store, the seeder and the tests use.
pub use agent_model::tags::{TagMember, content_hash as tag_content_hash};

/// Latest non-provisional version per family (plan §4 `latest_tags`).
pub async fn latest_tags(conn: &mut PgConnection) -> ApiResult<BTreeMap<String, i32>> {
    let rows: Vec<(String, i32)> = sqlx::query_as(
        "SELECT family, max(version) FROM layer_tag WHERE NOT provisional GROUP BY family",
    )
    .fetch_all(conn)
    .await?;
    Ok(rows.into_iter().collect())
}

/// The families `head` composes on, bottom up, each at its latest non-provisional tag (B6-4; port of the web
/// `layerStack` / `composesUnder`): base and every base_dept head (D116) under everything above them, and the
/// industry and industry_dept heads of the head's industry (a job head's industry is its `template_selection.industry`) under a higher scope. Families
/// with no tag yet are left out; the head itself never is in it.
pub async fn stack_for(conn: &mut PgConnection, head: &Layer) -> ApiResult<Vec<StackEntry>> {
    let rank = head.parsed_scope()?.rank();
    let industry = head.template_industry();
    let latest = latest_tags(&mut *conn).await?;
    let heads = sqlx::query_as::<_, Layer>(sqlx::AssertSqlSafe(format!(
        "SELECT {LAYER_COLUMNS} FROM layer WHERE family = key ORDER BY key"
    )))
    .fetch_all(&mut *conn)
    .await?;
    let mut below: Vec<(u8, String, i32)> = Vec::new();
    for h in &heads {
        let scope = h.parsed_scope()?;
        // D116 (S2): base and every base_dept head compose under a higher head whatever its industry; an
        // industry head ranks below base_dept, so it never stacks one.
        let under = scope.rank() < rank
            && (matches!(scope, Scope::Base | Scope::BaseDept)
                || (h.template_industry().is_some() && h.template_industry() == industry));
        if under && let Some(v) = latest.get(&h.key) {
            below.push((scope.rank(), h.key.clone(), *v));
        }
    }
    below.sort();
    Ok(below
        .into_iter()
        .map(|(_, layer, version)| StackEntry { layer, version })
        .collect())
}

/// Tag `family` (its head's key) in the caller's `agent_platform` transaction (plan B6-4): family lock; 404
/// `layer_not_found` (no such layer, or not a template layer); 422 `invalid_note`; 409 `nothing_to_tag` when
/// every current row is in the latest tag (B6-5); 422 `validation_red` with `items[]` when `guard` reports red
/// items. Version = latest non-provisional + 1 (a provisional v0 stays as it is); members = every current row
/// of every layer of the family; `stack` from [`stack_for`]; `content_hash` from [`tag_content_hash`]; the
/// `job_template_tag` projection (crate::platform::templates::project_job_template_tags); one `template.tag`
/// platform audit row.
///
/// `provisional` (B8 P7, a generator run's draft marker): cut the family's one provisional `v0` instead of a real
/// version. It replaces the family's previous `v0` (its members are re-snapshotted), skips the red-items guard (a
/// draft may be red) and keeps the version sequence untouched. A real tag (`provisional = false`) drops the family's
/// provisional `v0` in the same transaction: `v0 → @vN`.
pub async fn tag_family(
    conn: &mut PgConnection,
    actor: &Actor,
    family: &str,
    note: &str,
    by: &str,
    guard: &dyn GeneratorGuard,
    provisional: bool,
) -> ApiResult<TemplateTag> {
    let head = layers::layer_by_key(&mut *conn, family).await?;
    if !head.is_head() {
        return Err(ApiError::Missing(
            "layer_not_found",
            format!(
                "{family} is not a template layer; its family is {}.",
                head.family
            ),
        ));
    }
    if note.chars().count() > MAX_NOTE_CHARS {
        return Err(ApiError::Unprocessable(
            "invalid_note",
            format!("A tag note is at most {MAX_NOTE_CHARS} characters."),
        ));
    }
    lock_family(&mut *conn, family).await?;
    let untagged: i64 = sqlx::query_scalar("SELECT count(*) FROM layer_untagged WHERE family = $1")
        .bind(family)
        .fetch_one(&mut *conn)
        .await?;
    if untagged == 0 {
        return Err(ApiError::Conflict(
            "nothing_to_tag",
            format!("Every current revision of {family} is already in its latest tag."),
        ));
    }
    let red = if provisional {
        Vec::new()
    } else {
        guard.red_items(&mut *conn, family).await?
    };
    if !red.is_empty() {
        return Err(ApiError::detailed(
            StatusCode::UNPROCESSABLE_ENTITY,
            "validation_red",
            format!("The generator's validation of {family} is red; fix it before tagging."),
        )
        .with("items", red));
    }

    // the family's provisional v0 (a generator draft) is replaced: by the new v0, or by the real tag
    drop_provisional(&mut *conn, family).await?;
    let version: i32 = if provisional {
        0
    } else {
        sqlx::query_scalar(
            "SELECT coalesce(max(version), 0) + 1 FROM layer_tag WHERE family = $1 AND NOT provisional",
        )
        .bind(family)
        .fetch_one(&mut *conn)
        .await?
    };
    let current: Vec<(Uuid, String, String, String, String, String)> = sqlx::query_as(
        "SELECT r.id, l.key, r.kind, r.key, r.op, r.fields_hash \
           FROM layer_row r JOIN layer l ON l.id = r.layer_id \
          WHERE l.family = $1 AND r.is_current \
          ORDER BY l.key, r.kind, r.key",
    )
    .bind(family)
    .fetch_all(&mut *conn)
    .await?;
    let ids: Vec<Uuid> = current.iter().map(|m| m.0).collect();
    let members: Vec<TagMember> = current
        .into_iter()
        .map(|(_, layer, kind, key, op, fields_hash)| TagMember {
            layer,
            kind,
            key,
            op,
            fields_hash,
        })
        .collect();
    let content_hash = tag_content_hash(&members);
    let stack = serde_json::to_value(stack_for(&mut *conn, &head).await?)
        .map_err(|e| ApiError::Internal(e.into()))?;

    let tag = sqlx::query_as::<_, TemplateTag>(sqlx::AssertSqlSafe(format!(
        "INSERT INTO layer_tag (family, version, content_hash, note, provisional, stack, tagged_by) \
         VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING {TAG_COLUMNS}"
    )))
    .bind(family)
    .bind(version)
    .bind(&content_hash)
    .bind(note)
    .bind(provisional)
    .bind(&stack)
    .bind(by)
    .fetch_one(&mut *conn)
    .await?;
    sqlx::query(
        "INSERT INTO layer_tag_row (family, version, row_id) SELECT $1::text, $2::integer, unnest($3::uuid[])",
    )
    .bind(family)
    .bind(version)
    .bind(&ids)
    .execute(&mut *conn)
    .await?;
    // Ruling 17: every tag of a job / industry_dept family reaches B1's `job_template_tag` in this transaction.
    crate::platform::templates::project_job_template_tags(&mut *conn, family, by).await?;
    audit::record_platform(
        &mut *conn,
        actor,
        PlatformEvent::new("template.tag", family)
            .detail("family", family)
            .detail("version", version)
            .detail("provisional", provisional)
            .detail("members", ids.len())
            .detail("content_hash", content_hash.as_str())
            .detail("stack", stack),
    )
    .await?;
    Ok(tag)
}

/// Delete the family's provisional `v0`, if any (members first), through the 0195 definer function: the platform
/// role has no DELETE on tags. The 0041 triggers refuse deleting anything else.
async fn drop_provisional(conn: &mut PgConnection, family: &str) -> ApiResult<()> {
    sqlx::query("SELECT platform_drop_provisional_tag($1)")
        .bind(family)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// One tag; 404 `tag_not_found`.
pub async fn tag(conn: &mut PgConnection, family: &str, version: i32) -> ApiResult<TemplateTag> {
    sqlx::query_as::<_, TemplateTag>(sqlx::AssertSqlSafe(format!(
        "SELECT {TAG_COLUMNS} FROM layer_tag WHERE family = $1 AND version = $2"
    )))
    .bind(family)
    .bind(version)
    .fetch_optional(conn)
    .await?
    .ok_or_else(|| ApiError::Missing("tag_not_found", format!("No tag {family}@v{version}.")))
}

/// Every tag of a family (the provisional v0 included), oldest first.
pub async fn tags_of(conn: &mut PgConnection, family: &str) -> ApiResult<Vec<TemplateTag>> {
    Ok(
        sqlx::query_as::<_, TemplateTag>(sqlx::AssertSqlSafe(format!(
            "SELECT {TAG_COLUMNS} FROM layer_tag WHERE family = $1 ORDER BY version"
        )))
        .bind(family)
        .fetch_all(conn)
        .await?,
    )
}

/// The latest non-provisional tag of a family, if any.
pub async fn latest_tag(conn: &mut PgConnection, family: &str) -> ApiResult<Option<TemplateTag>> {
    Ok(
        sqlx::query_as::<_, TemplateTag>(sqlx::AssertSqlSafe(format!(
            "SELECT {TAG_COLUMNS} FROM layer_tag WHERE family = $1 AND NOT provisional \
          ORDER BY version DESC LIMIT 1"
        )))
        .bind(family)
        .fetch_optional(conn)
        .await?,
    )
}

/// The member rows of a tag, by (layer key, kind, key). Empty for an unknown tag (use [`tag`] for the 404).
pub async fn tag_members(
    conn: &mut PgConnection,
    family: &str,
    version: i32,
) -> ApiResult<Vec<KeyedRow>> {
    Ok(sqlx::query_as::<_, KeyedRow>(
        "SELECT r.id, r.layer_id, l.key AS layer_key, l.family, l.scope, r.kind, r.key, r.op, r.fields, \
                r.fields_hash, r.rev, r.author, r.created_at \
           FROM layer_tag_row m \
           JOIN layer_row r ON r.id = m.row_id \
           JOIN layer l ON l.id = r.layer_id \
          WHERE m.family = $1 AND m.version = $2 \
          ORDER BY l.key, r.kind, r.key",
    )
    .bind(family)
    .bind(version)
    .fetch_all(conn)
    .await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_model::canonical::{canonical_json, sha256_hex};
    use serde_json::json;

    fn member(layer: &str, kind: &str, key: &str, op: &str, fields: &Value) -> TagMember {
        TagMember {
            layer: layer.into(),
            kind: kind.into(),
            key: key.into(),
            op: op.into(),
            fields_hash: agent_model::canonical::fields_hash(fields),
        }
    }

    #[test]
    fn content_hash_is_order_independent_and_content_sensitive() {
        let a = member(
            "job/x",
            "entity",
            "Order",
            "add",
            &json!({"label": "Order"}),
        );
        let b = member(
            "job/x",
            "entity",
            "Party",
            "add",
            &json!({"label": "Party"}),
        );
        let h1 = tag_content_hash(&[a.clone(), b.clone()]);
        let h2 = tag_content_hash(&[b.clone(), a.clone()]);
        assert_eq!(h1, h2);
        assert!(
            h1.starts_with("sha256:") && h1.len() == "sha256:".len() + 64,
            "{h1}"
        );
        let b2 = member(
            "job/x",
            "entity",
            "Party",
            "add",
            &json!({"label": "Person"}),
        );
        assert_ne!(h1, tag_content_hash(&[a.clone(), b2]));
        let b3 = member("job/x", "entity", "Party", "tombstone", &json!({}));
        assert_ne!(h1, tag_content_hash(&[a, b3]));
        // the empty tag has a stable hash too
        assert_eq!(tag_content_hash(&[]), tag_content_hash(&[]));
    }

    #[test]
    fn content_hash_is_the_canonical_member_list() {
        let a = member("base", "job", "faq_agent", "add", &json!({"label": "FAQ"}));
        let want = format!(
            "sha256:{}",
            sha256_hex(&canonical_json(&json!([{
                "fields_hash": a.fields_hash,
                "key": "faq_agent",
                "kind": "job",
                "layer": "base",
                "op": "add",
            }])))
        );
        assert_eq!(tag_content_hash(std::slice::from_ref(&a)), want);
    }
}
