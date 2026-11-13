//! `objectContext` (plan B6 §5, Revision R9; port of the web `objectContextFrom`): where an object lives outside its
//! own rows.
//!
//! - `layers`: every template-layer item that defines the object (`object` row) or references it (`order.cancel`,
//!   `object delivery`; gate questions excluded), with its tag history (`since`, `changed_in`, `removed_after`) from
//!   the non-provisional tags of the layer's family and `pending` from `layer_untagged`.
//! - `systems`: the newest published (not `draft`) version of every B7 connector template with an operation on the
//!   object, its status map and its tenant instances (read tenant by tenant as `agent_app`, the B7
//!   `coverage::tenant_rows` pattern, R3), plus whatever the `ObjectContextSources` hook adds.
//! - `activities`: the hook's (B8); `[]` until then.
use std::collections::{BTreeMap, BTreeSet};

use agent_model::kinds::{self, Kind};
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use super::catalogue::{detail, mentions, scope_rank};
use crate::connectors::instances::cmp_version;
use crate::error::ApiResult;
use crate::platform::db::tags;
use crate::platform_db;
use crate::state::AppState;
use crate::tenancy::set_tenant;

/// One revision of a candidate item: a tag member (`version` set) or a current row (`version` none).
#[derive(Debug, Clone, sqlx::FromRow)]
struct Touch {
    version: Option<i32>,
    layer_key: String,
    kind: String,
    key: String,
    op: String,
    fields: Value,
    fields_hash: String,
}

/// The candidate filter: the object's own row, or any non-gate row whose key or summary contains the key (refined
/// in Rust by [`mentions`]).
const CANDIDATE: &str = "r.kind <> 'gate_question' \
     AND ((r.kind = 'object' AND r.key = $1) \
          OR strpos(r.key || ' ' || coalesce(r.fields->>'summary', ''), $1) > 0)";

#[derive(Debug, Clone, sqlx::FromRow)]
struct LayerInfo {
    key: String,
    scope: String,
    label: Option<String>,
    family: String,
}

#[derive(Default)]
struct Acc {
    /// (version, fields_hash, detail, label) of every tag carrying the item, oldest first.
    carried: Vec<(i32, String, String, String)>,
    /// The current row: (op, detail, label).
    current: Option<(String, String, String)>,
}

/// The `layers` section.
pub async fn layer_touches(conn: &mut PgConnection, object: &str) -> ApiResult<Vec<Value>> {
    let members: Vec<Touch> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT m.version, l.key AS layer_key, r.kind, r.key, r.op, r.fields, r.fields_hash \
           FROM layer_tag_row m \
           JOIN layer_tag t ON t.family = m.family AND t.version = m.version \
           JOIN layer_row r ON r.id = m.row_id \
           JOIN layer l ON l.id = r.layer_id \
          WHERE NOT t.provisional AND l.scope <> 'profile' AND {CANDIDATE} \
          ORDER BY l.key, r.kind, r.key, m.version"
    )))
    .bind(object)
    .fetch_all(&mut *conn)
    .await?;
    let current: Vec<Touch> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT NULL::int4 AS version, r.layer_key, r.kind, r.key, r.op, r.fields, r.fields_hash \
           FROM layer_head r \
          WHERE r.scope <> 'profile' AND {CANDIDATE}"
    )))
    .bind(object)
    .fetch_all(&mut *conn)
    .await?;
    let pending: BTreeSet<(String, String, String)> =
        sqlx::query_as::<_, (String, String, String)>(
            "SELECT layer_key, kind, key FROM layer_untagged WHERE scope <> 'profile'",
        )
        .fetch_all(&mut *conn)
        .await?
        .into_iter()
        .collect();
    let layers: Vec<LayerInfo> =
        sqlx::query_as("SELECT key, scope, label, family FROM layer WHERE scope <> 'profile'")
            .fetch_all(&mut *conn)
            .await?;
    let latest = tags::latest_tags(&mut *conn).await?;

    let mut acc: BTreeMap<(String, String, String), Acc> = BTreeMap::new();
    for t in members.iter().chain(current.iter()) {
        let Some(kind) = Kind::parse(&t.kind) else {
            continue;
        };
        let d = detail(kind, &t.fields);
        let label = kinds::label(kind, &t.key, &t.fields);
        let a = acc
            .entry((t.layer_key.clone(), t.kind.clone(), t.key.clone()))
            .or_default();
        match t.version {
            Some(v) if t.op != "tombstone" => a.carried.push((v, t.fields_hash.clone(), d, label)),
            Some(_) => {}
            None => a.current = Some((t.op.clone(), d, label)),
        }
    }

    let mut out: Vec<(u8, String, Value)> = Vec::new();
    for ((layer_key, kind, key), a) in acc {
        let Some(l) = layers.iter().find(|l| l.key == layer_key) else {
            continue;
        };
        let latest_tag = latest.get(&l.family).copied();
        let is_pending = pending.contains(&(layer_key.clone(), kind.clone(), key.clone()));
        let live = a.current.as_ref().filter(|(op, _, _)| op != "tombstone");
        let defines = kind == "object" && key == object;
        let refs = !defines
            && (mentions(&key, object)
                || a.carried
                    .iter()
                    .any(|c| mentions(&format!("{key} {}", c.2), object))
                || live.is_some_and(|c| mentions(&format!("{key} {}", c.1), object)));
        if !defines && !refs {
            continue;
        }
        let (since, changed_in, removed_after, detail, label) = match a.carried.first() {
            None => {
                // never tagged: only a pending draft that adds the object itself shows (web: generator Package step)
                let Some((_, d, lbl)) = live.filter(|_| defines && is_pending) else {
                    continue;
                };
                (
                    latest_tag.unwrap_or(0) + 1,
                    Vec::new(),
                    None,
                    d.clone(),
                    lbl.clone(),
                )
            }
            Some(first) => {
                let changed: Vec<i32> = a
                    .carried
                    .windows(2)
                    .filter(|w| w[0].1 != w[1].1)
                    .map(|w| w[1].0)
                    .collect();
                let last = a.carried.last().unwrap_or(first);
                let removed = latest_tag.filter(|l| last.0 < *l).map(|_| last.0);
                let (d, lbl) = match live.filter(|_| is_pending) {
                    Some((_, d, lbl)) => (d.clone(), lbl.clone()),
                    None => (last.2.clone(), last.3.clone()),
                };
                (first.0, changed, removed, d, lbl)
            }
        };
        out.push((
            scope_rank(&l.scope),
            layer_key.clone(),
            json!({
                "layer_key": layer_key,
                "scope": l.scope,
                "layer_label": l.label.clone().unwrap_or_else(|| layer_key.clone()),
                "latest_tag": latest_tag,
                "role": if defines { "defines" } else { "references" },
                "kind": kind,
                "key": key,
                "label": label,
                "detail": detail,
                "since": since,
                "changed_in": changed_in,
                "removed_after": removed_after,
                "pending": is_pending,
            }),
        ));
    }
    // broadest scope first, then layer key; items keep (kind, key) order within a layer
    out.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
    Ok(out.into_iter().map(|(_, _, v)| v).collect())
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct TemplateRow {
    key: String,
    version: String,
    label: String,
    vendor: String,
    system_class: String,
    platform_connector: bool,
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct InstanceRow {
    id: String,
    label: String,
    env: String,
    stub: bool,
    template_key: String,
}

/// The newest of each key (B7 `cmp_version`).
fn newest(rows: Vec<TemplateRow>) -> Vec<TemplateRow> {
    let mut by_key: BTreeMap<String, TemplateRow> = BTreeMap::new();
    for t in rows {
        let newer = by_key
            .get(&t.key)
            .is_none_or(|cur| cmp_version(&t.version, &cur.version).is_gt());
        if newer {
            by_key.insert(t.key.clone(), t);
        }
    }
    by_key.into_values().collect()
}

/// The status map of a template's ops on the object: one row per distinct system status, `invalid_state*` left
/// out (web `objectContextFrom`).
fn status_map(maps: &[Value]) -> Vec<Value> {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut out = Vec::new();
    for m in maps.iter().filter_map(Value::as_array).flatten() {
        let system = m.get("system").and_then(Value::as_str).unwrap_or_default();
        let canonical = m
            .get("canonical")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if canonical.starts_with("invalid_state") || !seen.insert(system.to_owned()) {
            continue;
        }
        out.push(json!({"system": system, "canonical": canonical}));
    }
    out
}

/// `order.read`, `case.create{return}`.
fn op_string(object: &str, verb: &str, variant: &str) -> String {
    if variant == "*" {
        format!("{object}.{verb}")
    } else {
        format!("{object}.{verb}{{{variant}}}")
    }
}

/// The `systems` section. Runs last in the read transaction: the tenant reads leave it as `agent_app`.
pub async fn systems(
    st: &AppState,
    conn: &mut PgConnection,
    object: &str,
) -> ApiResult<Vec<Value>> {
    let templates: Vec<TemplateRow> = sqlx::query_as(
        "SELECT t.key, t.version, t.label, t.vendor, t.system_class, t.platform_connector \
           FROM connector_template t \
          WHERE t.status <> 'draft' \
            AND EXISTS (SELECT 1 FROM connector_template_op o \
                         WHERE o.key = t.key AND o.version = t.version AND o.object = $1)",
    )
    .bind(object)
    .fetch_all(&mut *conn)
    .await?;
    let templates = newest(templates);
    let mut built: Vec<(TemplateRow, Vec<String>, Vec<Value>)> = Vec::new();
    for t in templates {
        let ops: Vec<(String, String, Value)> = sqlx::query_as(
            "SELECT verb, variant, status_map FROM connector_template_op \
              WHERE key = $1 AND version = $2 AND object = $3 ORDER BY verb, variant",
        )
        .bind(&t.key)
        .bind(&t.version)
        .bind(object)
        .fetch_all(&mut *conn)
        .await?;
        let names: Vec<String> = ops
            .iter()
            .map(|(verb, variant, _)| op_string(object, verb, variant))
            .collect();
        let maps: Vec<Value> = ops.into_iter().map(|(_, _, m)| m).collect();
        built.push((t, names, status_map(&maps)));
    }
    let keys: Vec<String> = built.iter().map(|(t, _, _)| t.key.clone()).collect();
    let instances = if keys.is_empty() {
        Vec::new()
    } else {
        tenant_instances(conn, &keys).await?
    };
    let mut out: Vec<Value> = built
        .into_iter()
        .map(|(t, ops, map)| {
            let inst: Vec<Value> = instances
                .iter()
                .filter(|(_, _, i)| i.template_key == t.key)
                .map(|(tenant_id, tenant_name, i)| {
                    json!({
                        "id": i.id,
                        "label": i.label,
                        "tenant_id": tenant_id,
                        "tenant_name": tenant_name,
                        "env": i.env,
                        "stub": i.stub,
                    })
                })
                .collect();
            json!({
                "template_key": t.key,
                "version": t.version,
                "label": t.label,
                "vendor": t.vendor,
                "system_class": t.system_class,
                "platform_connector": t.platform_connector,
                "ops": ops,
                "status_map": map,
                "instances": inst,
            })
        })
        .collect();
    let len = |v: &Value, k: &str| v.get(k).and_then(Value::as_array).map_or(0, Vec::len);
    out.sort_by(|a, b| {
        len(b, "status_map")
            .cmp(&len(a, "status_map"))
            .then(len(b, "ops").cmp(&len(a, "ops")))
    });
    out.extend(st.object_context.systems(object));
    Ok(out)
}

/// The platform-scope instances of `keys` in every tenant: the tenant list under `agent_platform`
/// (`agent_list_tenants`), then one RLS-scoped read per tenant as `agent_app`.
async fn tenant_instances(
    conn: &mut PgConnection,
    keys: &[String],
) -> ApiResult<Vec<(String, String, InstanceRow)>> {
    platform_db::enter(&mut *conn).await?;
    let tenants: Vec<(Uuid, String)> = sqlx::query_as("SELECT id, name FROM agent_list_tenants()")
        .fetch_all(&mut *conn)
        .await?;
    platform_db::leave(&mut *conn).await?;
    let mut out = Vec::new();
    for (id, name) in tenants {
        set_tenant(&mut *conn, id).await?;
        let rows: Vec<InstanceRow> = sqlx::query_as(
            "SELECT id, label, env, stub, template_key FROM connector_instance \
              WHERE template_scope = 'platform' AND template_key = ANY($1) ORDER BY id",
        )
        .bind(keys)
        .fetch_all(&mut *conn)
        .await?;
        out.extend(rows.into_iter().map(|r| (id.to_string(), name.clone(), r)));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(key: &str, version: &str) -> TemplateRow {
        TemplateRow {
            key: key.into(),
            version: version.into(),
            label: key.into(),
            vendor: String::new(),
            system_class: String::new(),
            platform_connector: false,
        }
    }

    #[test]
    fn newest_version_per_template() {
        let got: Vec<(String, String)> = newest(vec![
            t("shopify_admin", "2026-07.1"),
            t("demo_retail", "5"),
            t("shopify_admin", "2026-10.1"),
            t("demo_retail", "10"),
            t("demo_retail", "3"),
        ])
        .into_iter()
        .map(|r| (r.key, r.version))
        .collect();
        assert_eq!(
            got,
            vec![
                ("demo_retail".to_owned(), "10".to_owned()),
                ("shopify_admin".to_owned(), "2026-10.1".to_owned())
            ]
        );
    }

    #[test]
    fn status_map_dedups_and_drops_invalid_state() {
        let maps = vec![
            json!([{"system": "Open", "canonical": "processing"}, {"system": "x", "canonical": "invalid_state:x"}]),
            json!([{"system": "Open", "canonical": "other"}, {"system": "Done", "canonical": "delivered"}]),
        ];
        assert_eq!(
            status_map(&maps),
            vec![
                json!({"system": "Open", "canonical": "processing"}),
                json!({"system": "Done", "canonical": "delivered"})
            ]
        );
    }

    #[test]
    fn op_strings() {
        assert_eq!(op_string("case", "create", "*"), "case.create");
        assert_eq!(op_string("case", "create", "return"), "case.create{return}");
    }
}
