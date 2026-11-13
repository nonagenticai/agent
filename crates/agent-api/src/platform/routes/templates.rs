//! B6b platform-model area `templates` (plan B6 §5, T11 KH bucket of Revision 2026-10-22): P6/P7.
//!
//! - `GET /api/platform/templates` (templateCatalog) → `{layers: TemplateLayerSummary[], tenant_lag:
//!   TenantPinLag[]}`: one summary per template family (a head: scope ≠ profile, family = key), by key; tags newest
//!   first; `draft_revisions` = the family's untagged current rows (B6-5, `by_name` = `agent_actor_name`); pins and
//!   `tenant_lag` from every tenant's `jobs.pins` (R3: tenants through `agent_list_tenants()` as `agent_platform`,
//!   then each tenant's jobs as `agent_app` under RLS). `generator` = `null` until B8.
//! - `GET /api/platform/templates/{layer}/{tag}` (templateTagDetail; `layer` = dotted slug, `tag` = `v{n}`) →
//!   `TemplateTagDetail`: items / diff vs the previous tag / versions / previous / stack / composed_on_by / catch_up
//!   over the tag members (`agent_model::tags`, region-keyed contents, D111). 404 `layer_not_found`, `tag_not_found`.
//! - `POST /api/platform/templates/{layer}/tags` body `{note}` (tagLayerRevisions) → 201 `TemplateTag`:
//!   `db::tags::tag_family` with the AppState `GeneratorGuard` (404 `layer_not_found`; 409 `nothing_to_tag`; 422
//!   `validation_red`, `invalid_note`, and the projection's `hierarchy_incomplete` / `unknown_region`); its
//!   `template.tag` audit row is the one row of the write.
//!
//! - `POST /api/platform/templates/{layer}/tags/{tag}/retire` and `…/restore` (`tag` = `v{n}`) → `{layer_key, version,
//!   retired}`: take a registered job template (`job_template_tag`) back from the tenant catalogue and from new jobs
//!   (422 `template_retired` at creation); jobs that pin it keep working. 404 `layer_not_found`, `template_not_found`
//!   (the tag has no `job_template_tag` row); 409 `no_change`. Audit `job_template.retire` / `job_template.restore`.
//!
//! The P7 `stack` is the tag's recorded stack (the families `tag_family` cut it on: base, the head's industry and
//! departments, D115) plus the head itself (`here`); `composed_on_by` is `agent_model::tags::composed_on_by` over the
//! template heads.
use std::collections::{BTreeMap, BTreeSet};

use agent_model::compile::Row as ModelRow;
use agent_model::kinds::{Kind, Op};
use agent_model::layer::{LayerKey, Scope};
use agent_model::tags::{self as model_tags, StackLayer, TagItem};
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use super::{Who, layer_from_slug, missing, read_tx, write_tx};
use crate::audit::{self, PlatformEvent};
use crate::error::{ApiError, ApiJson, ApiPath, ApiResult};
use crate::platform::db::layers::{self, Layer};
use crate::platform::db::rows::{self, DraftRow, KeyedRow};
use crate::platform::db::tags::{self, StackEntry, TemplateTag};
use crate::platform_db;
use crate::state::AppState;
use crate::tenancy::{SuperAdmin, set_tenant};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/platform/templates", get(template_catalog))
        .route(
            "/api/platform/templates/{layer}/tags",
            post(tag_layer_revisions),
        )
        .route(
            "/api/platform/templates/{layer}/{tag}",
            get(template_tag_detail),
        )
        .route(
            "/api/platform/templates/{layer}/tags/{tag}/retire",
            post(retire_template),
        )
        .route(
            "/api/platform/templates/{layer}/tags/{tag}/restore",
            post(restore_template),
        )
}

/// UI `ROLLOUT` (web `fixtures/model.ts`, D26).
pub fn rollout_label(wave: Option<i16>) -> Option<&'static str> {
    match wave? {
        1 => Some("Customer care"),
        2 => Some("IT service desk"),
        3 => Some("Other job areas"),
        _ => None,
    }
}

/// `v{n}` → n; 404 `tag_not_found` otherwise.
pub fn parse_tag(layer: &str, raw: &str) -> ApiResult<i32> {
    raw.strip_prefix('v')
        .filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|n| n.parse::<i32>().ok())
        .ok_or_else(|| missing("tag_not_found", format!("{layer} has no tag “{raw}”.")))
}

// ---- reading rows as tag items ------------------------------------------------------------------------------------

fn model_row(
    id: Uuid,
    layer_id: Uuid,
    kind: &str,
    key: &str,
    op: &str,
    fields: &Value,
    rev: i32,
) -> ApiResult<ModelRow> {
    let corrupt = |what: String| ApiError::Internal(anyhow::anyhow!("platform store: {what}"));
    let kind = Kind::parse(kind).ok_or_else(|| corrupt(format!("row {id} has kind {kind:?}")))?;
    let op = match op {
        "add" => Op::Add,
        "patch" => Op::Patch,
        "tombstone" => Op::Tombstone,
        other => return Err(corrupt(format!("row {id} has op {other:?}"))),
    };
    Ok(ModelRow {
        id: id.to_string(),
        layer_id: layer_id.to_string(),
        kind,
        key: key.to_owned(),
        op,
        fields: fields.clone(),
        rev,
    })
}

/// The region of a layer key (`…/region/<r>`, D111), if any.
fn region_of_key(layer_key: &str) -> Option<String> {
    LayerKey::parse_label(layer_key).ok()?.region
}

/// The contents of a family's rows (tag members or current rows) as UI tag items: head layer first, then the other
/// layers by key, each by (kind, key) — the order of `platform::templates::tag_diff` — with region sub-layer rows
/// kept apart (`TagItem::region`).
pub fn contents_of(family: &str, members: &[KeyedRow]) -> ApiResult<Vec<TagItem>> {
    let mut sorted: Vec<&KeyedRow> = members.iter().collect();
    sorted.sort_by(|a, b| {
        (
            a.layer_key != family,
            a.layer_key.as_str(),
            a.kind.as_str(),
            a.key.as_str(),
        )
            .cmp(&(
                b.layer_key != family,
                b.layer_key.as_str(),
                b.kind.as_str(),
                b.key.as_str(),
            ))
    });
    let rows = sorted
        .iter()
        .map(|m| model_row(m.id, m.layer_id, &m.kind, &m.key, &m.op, &m.fields, m.rev))
        .collect::<ApiResult<Vec<_>>>()?;
    let region: BTreeMap<String, String> = sorted
        .iter()
        .filter_map(|m| Some((m.layer_id.to_string(), region_of_key(&m.layer_key)?)))
        .collect();
    Ok(model_tags::contents_by_region(&rows, |r| {
        region.get(&r.layer_id).cloned()
    }))
}

/// Every tag of a family's contents, by version.
async fn contents_by_version(
    conn: &mut PgConnection,
    family: &str,
    versions: &[i32],
) -> ApiResult<BTreeMap<i32, Vec<TagItem>>> {
    let mut out = BTreeMap::new();
    for v in versions {
        let members = tags::tag_members(&mut *conn, family, *v).await?;
        out.insert(*v, contents_of(family, &members)?);
    }
    Ok(out)
}

/// The display names of `subs` (`agent_actor_name`, else the sub itself).
async fn names(
    conn: &mut PgConnection,
    subs: &BTreeSet<String>,
) -> ApiResult<BTreeMap<String, String>> {
    let list: Vec<String> = subs.iter().cloned().collect();
    let rows: Vec<(String, Option<String>)> =
        sqlx::query_as("SELECT s, agent_actor_name(s) FROM unnest($1::text[]) AS s")
            .bind(&list)
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

fn name_of(names: &BTreeMap<String, String>, sub: &str) -> String {
    names.get(sub).cloned().unwrap_or_else(|| sub.to_owned())
}

// ---- the UI shapes ------------------------------------------------------------------------------------------------

/// UI `TemplateTag`.
#[derive(Debug, Clone, Serialize)]
pub struct TagView {
    pub layer_key: String,
    pub version: i32,
    pub content_hash: String,
    pub tagged_by: String,
    pub tagged_by_name: String,
    pub tagged_at: DateTime<Utc>,
    pub note: String,
    pub provisional: bool,
    pub item_count: usize,
}

fn tag_view(t: &TemplateTag, item_count: usize, names: &BTreeMap<String, String>) -> TagView {
    TagView {
        layer_key: t.family.clone(),
        version: t.version,
        content_hash: t.content_hash.clone(),
        tagged_by: t.tagged_by.clone(),
        tagged_by_name: name_of(names, &t.tagged_by),
        tagged_at: t.tagged_at,
        note: t.note.clone(),
        provisional: t.provisional,
        item_count,
    }
}

/// UI `DraftRevision`.
#[derive(Debug, Clone, Serialize)]
pub struct DraftView {
    pub rev: i32,
    pub op: String,
    pub item: TagItem,
    pub by: String,
    pub by_name: String,
    pub at: DateTime<Utc>,
}

fn draft_view(d: &DraftRow, names: &BTreeMap<String, String>) -> ApiResult<DraftView> {
    let row = model_row(d.id, d.layer_id, &d.kind, &d.key, &d.op, &d.fields, d.rev)?;
    Ok(DraftView {
        rev: d.rev,
        op: d.draft_op.clone(),
        item: TagItem {
            region: region_of_key(&d.layer_key),
            ..TagItem::of_row(&row)
        },
        by: d.author.clone(),
        by_name: name_of(names, &d.author),
        at: d.created_at,
    })
}

/// UI `TemplatePinRow`.
#[derive(Debug, Clone, Serialize)]
pub struct PinView {
    pub tenant_id: String,
    pub tenant_name: String,
    pub tenant_status: String,
    pub job: String,
    pub job_label: String,
    pub version: i32,
    pub behind: i32,
}

/// One tenant job and its pins (`jobs.pins`, B1 0033).
#[derive(Debug, Clone)]
pub struct JobPins {
    pub tenant_id: Uuid,
    pub tenant_name: String,
    pub tenant_status: String,
    pub job: String,
    pub job_label: String,
    pub pins: Vec<StackEntry>,
}

/// Every tenant job with its pins, by tenant name then job key (R3, the B7 / `gates::pins` way): the tenant list as
/// `agent_platform`, each tenant's jobs as `agent_app` under RLS, then back to `agent_platform` for the rest of the
/// caller's (read-only) transaction.
pub async fn tenant_job_pins(conn: &mut PgConnection) -> ApiResult<Vec<JobPins>> {
    let mut tenants: Vec<(Uuid, String, String)> =
        sqlx::query_as("SELECT id, name, status FROM agent_list_tenants()")
            .fetch_all(&mut *conn)
            .await?;
    tenants.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(&b.0)));
    platform_db::leave(&mut *conn).await?;
    let mut out = Vec::new();
    for (id, name, status) in tenants {
        set_tenant(&mut *conn, id).await?;
        let jobs: Vec<(String, String, Value)> =
            sqlx::query_as("SELECT key, label, pins FROM jobs ORDER BY key")
                .fetch_all(&mut *conn)
                .await?;
        for (job, job_label, pins) in jobs {
            let pins: Vec<StackEntry> = serde_json::from_value(pins).unwrap_or_else(|e| {
                tracing::warn!(tenant = %id, %job, error = %e, "job_pins_unreadable");
                Vec::new()
            });
            out.push(JobPins {
                tenant_id: id,
                tenant_name: name.clone(),
                tenant_status: status.clone(),
                job,
                job_label,
                pins,
            });
        }
    }
    platform_db::enter(&mut *conn).await?;
    Ok(out)
}

/// The pins on `family`, by tenant name then job key; `behind` = `latest - version` (0 = on the latest).
pub fn pins_of(jobs: &[JobPins], family: &str, latest: Option<i32>) -> Vec<PinView> {
    let latest = latest.unwrap_or(0);
    jobs.iter()
        .flat_map(|j| {
            j.pins
                .iter()
                .filter(move |p| p.layer == family)
                .map(move |p| PinView {
                    tenant_id: j.tenant_id.to_string(),
                    tenant_name: j.tenant_name.clone(),
                    tenant_status: j.tenant_status.clone(),
                    job: j.job.clone(),
                    job_label: j.job_label.clone(),
                    version: p.version,
                    behind: (latest - p.version).max(0),
                })
        })
        .collect()
}

/// A template head: its own family and not a profile layer.
fn is_template(l: &Layer) -> bool {
    l.is_head() && l.scope != Scope::Profile.as_str()
}

/// The parts of a `TemplateLayerSummary` every response shares.
struct Family {
    head: Layer,
    tags: Vec<TemplateTag>,
    contents: BTreeMap<i32, Vec<TagItem>>,
    drafts: Vec<DraftRow>,
}

impl Family {
    fn latest(&self) -> Option<&TemplateTag> {
        self.tags
            .iter()
            .filter(|t| !t.provisional)
            .max_by_key(|t| t.version)
    }

    fn item_count(&self, version: i32) -> usize {
        self.contents.get(&version).map_or(0, Vec::len)
    }

    fn subs(&self) -> BTreeSet<String> {
        self.tags
            .iter()
            .map(|t| t.tagged_by.clone())
            .chain(self.drafts.iter().map(|d| d.author.clone()))
            .collect()
    }

    /// The summary fields without `tags`, `pins`, `draft_revisions` (P7 `layer`).
    fn head_view(
        &self,
        names: &BTreeMap<String, String>,
        pins: &[PinView],
    ) -> serde_json::Map<String, Value> {
        let latest = self
            .latest()
            .map(|t| tag_view(t, self.item_count(t.version), names));
        let mut m = serde_json::Map::new();
        m.insert("layer_key".into(), json!(self.head.key));
        m.insert("scope".into(), json!(self.head.scope));
        m.insert(
            "label".into(),
            json!(
                self.head
                    .label
                    .clone()
                    .unwrap_or_else(|| self.head.key.clone())
            ),
        );
        m.insert("industry".into(), json!(self.head.template_industry()));
        m.insert("rollout_wave".into(), json!(self.head.rollout_wave));
        m.insert(
            "rollout_label".into(),
            json!(rollout_label(self.head.rollout_wave)),
        );
        m.insert("latest".into(), json!(latest));
        m.insert("generator".into(), Value::Null);
        m.insert("pinned_jobs".into(), json!(pins.len()));
        m.insert(
            "behind_jobs".into(),
            json!(pins.iter().filter(|p| p.behind > 0).count()),
        );
        m
    }
}

async fn load_family(conn: &mut PgConnection, head: Layer) -> ApiResult<Family> {
    let tags = tags::tags_of(&mut *conn, &head.key).await?;
    let versions: Vec<i32> = tags.iter().map(|t| t.version).collect();
    let contents = contents_by_version(&mut *conn, &head.key, &versions).await?;
    let drafts = rows::untagged_rows(&mut *conn, &head.key).await?;
    Ok(Family {
        head,
        tags,
        contents,
        drafts,
    })
}

// ---- handlers -----------------------------------------------------------------------------------------------------

async fn template_catalog(
    SuperAdmin(_c): SuperAdmin,
    State(st): State<AppState>,
) -> ApiResult<Json<Value>> {
    let mut tx = read_tx(&st).await?;
    let mut families = Vec::new();
    for head in layers::layers(&mut tx)
        .await?
        .into_iter()
        .filter(is_template)
    {
        families.push(load_family(&mut tx, head).await?);
    }
    let subs: BTreeSet<String> = families.iter().flat_map(Family::subs).collect();
    let names = names(&mut tx, &subs).await?;
    let mut retired: BTreeMap<String, Vec<i32>> = BTreeMap::new();
    for f in &families {
        retired.insert(
            f.head.key.clone(),
            retired_versions(&mut tx, &f.head.key).await?,
        );
    }
    let jobs = tenant_job_pins(&mut tx).await?;
    drop(tx);

    let mut out_layers = Vec::new();
    // tenant id → (first-seen order, lag row)
    let mut lag: BTreeMap<String, (usize, Value)> = BTreeMap::new();
    for f in &families {
        let latest = f.latest().map(|t| t.version);
        let pins = pins_of(&jobs, &f.head.key, latest);
        let mut m = f.head_view(&names, &pins);
        let mut tag_list: Vec<TagView> = f
            .tags
            .iter()
            .map(|t| tag_view(t, f.item_count(t.version), &names))
            .collect();
        tag_list.sort_by_key(|a| std::cmp::Reverse(a.version));
        m.insert("tags".into(), json!(tag_list));
        let drafts = f
            .drafts
            .iter()
            .map(|d| draft_view(d, &names))
            .collect::<ApiResult<Vec<_>>>()?;
        m.insert("draft_revisions".into(), json!(drafts));
        m.insert(
            "retired_versions".into(),
            json!(retired.get(&f.head.key).cloned().unwrap_or_default()),
        );
        for p in &pins {
            let n = lag.len();
            let row = &mut lag
                .entry(p.tenant_id.clone())
                .or_insert_with(|| {
                    (
                        n,
                        json!({
                            "tenant_id": p.tenant_id,
                            "tenant_name": p.tenant_name,
                            "tenant_status": p.tenant_status,
                            "behind": [],
                        }),
                    )
                })
                .1;
            if p.behind > 0
                && let Some(list) = row["behind"].as_array_mut()
            {
                list.push(json!({
                    "job": p.job,
                    "layer_key": f.head.key,
                    "pinned": p.version,
                    "latest": latest,
                }));
            }
        }
        m.insert("pins".into(), json!(pins));
        out_layers.push(Value::Object(m));
    }
    let mut lag: Vec<(usize, Value)> = lag.into_values().collect();
    lag.sort_by_key(|(n, _)| *n);
    Ok(Json(json!({
        "layers": out_layers,
        "tenant_lag": lag.into_iter().map(|(_, v)| v).collect::<Vec<_>>(),
    })))
}

/// The template head named by a URL slug; 404 `layer_not_found` (no such layer, or not a template layer).
async fn template_head(conn: &mut PgConnection, slug: &str) -> ApiResult<Layer> {
    let key = layer_from_slug(slug);
    let head = layers::find_layer_by_key(&mut *conn, &key)
        .await?
        .filter(is_template)
        .ok_or_else(|| missing("layer_not_found", format!("No template layer “{key}”.")))?;
    Ok(head)
}

async fn template_tag_detail(
    SuperAdmin(_c): SuperAdmin,
    State(st): State<AppState>,
    ApiPath((slug, raw_tag)): ApiPath<(String, String)>,
) -> ApiResult<Json<Value>> {
    let mut tx = read_tx(&st).await?;
    let head = template_head(&mut tx, &slug).await?;
    let family = head.key.clone();
    let version = parse_tag(&family, &raw_tag)?;
    let tag = tags::tag(&mut tx, &family, version).await?;
    let f = load_family(&mut tx, head).await?;
    let retired = retired_versions(&mut tx, &family).await?;

    // the composition stack: the tag's recorded stack (bottom up) and the head itself
    let stack: Vec<StackEntry> = serde_json::from_value(tag.stack.clone())
        .map_err(|e| ApiError::Internal(anyhow::anyhow!("tag {family}@v{version} stack: {e}")))?;
    let all_heads: Vec<Layer> = layers::layers(&mut tx)
        .await?
        .into_iter()
        .filter(is_template)
        .collect();
    let latest_all = tags::latest_tags(&mut tx).await?;
    let mut subs = f.subs();
    subs.insert(tag.tagged_by.clone());
    let names = names(&mut tx, &subs).await?;
    let jobs = tenant_job_pins(&mut tx).await?;
    drop(tx);

    let by_key: BTreeMap<&str, &Layer> = all_heads.iter().map(|l| (l.key.as_str(), l)).collect();
    let mut stack_view: Vec<Value> = stack
        .iter()
        .map(|s| {
            let l = by_key.get(s.layer.as_str());
            json!({
                "layer_key": s.layer,
                "scope": l.map(|l| l.scope.clone()),
                "label": l.and_then(|l| l.label.clone()).unwrap_or_else(|| s.layer.clone()),
                "latest": latest_all.get(&s.layer),
                "here": false,
            })
        })
        .collect();
    stack_view.push(json!({
        "layer_key": family,
        "scope": f.head.scope,
        "label": f.head.label.clone().unwrap_or_else(|| family.clone()),
        "latest": latest_all.get(&family),
        "here": true,
    }));

    let stack_layers: Vec<StackLayer> = all_heads
        .iter()
        .filter_map(|l| {
            Some(StackLayer {
                layer_key: l.key.clone(),
                scope: l.parsed_scope().ok()?,
                industry: l.template_industry().map(str::to_owned),
            })
        })
        .collect();
    let composed_on_by: Vec<String> = stack_layers
        .iter()
        .find(|s| s.layer_key == family)
        .map(|me| {
            model_tags::composed_on_by(&stack_layers, me)
                .into_iter()
                .map(|s| s.layer_key.clone())
                .collect()
        })
        .unwrap_or_default();

    let versions: Vec<i32> = f.tags.iter().map(|t| t.version).collect();
    let previous = versions.iter().copied().filter(|v| *v < version).max();
    let empty: Vec<TagItem> = Vec::new();
    let items = f.contents.get(&version).unwrap_or(&empty);
    let before = previous.and_then(|p| f.contents.get(&p)).unwrap_or(&empty);
    let diff = model_tags::diff(before, items);
    let latest = f.latest().map(|t| t.version);
    let pins = pins_of(&jobs, &family, latest);
    let pinned: Vec<i32> = pins.iter().map(|p| p.version).collect();
    let catch_up = match latest {
        Some(l) => model_tags::catch_up(&pinned, l, |v| {
            f.contents.get(&v).cloned().unwrap_or_default()
        }),
        None => Vec::new(),
    };
    let tag_list: Vec<TagView> = f
        .tags
        .iter()
        .map(|t| tag_view(t, f.item_count(t.version), &names))
        .collect();
    let drafts = f
        .drafts
        .iter()
        .map(|d| draft_view(d, &names))
        .collect::<ApiResult<Vec<_>>>()?;
    Ok(Json(json!({
        "layer": Value::Object(f.head_view(&names, &pins)),
        "tag": tag_view(&tag, f.item_count(version), &names),
        "versions": versions,
        "previous": previous,
        "items": items,
        "diff_vs_previous": diff,
        "pinned_on_this_tag": pins.iter().filter(|p| p.version == version).count(),
        "pins": pins,
        "tags": tag_list,
        "draft_revisions": drafts,
        "stack": stack_view,
        "composed_on_by": composed_on_by,
        "catch_up": catch_up,
        "retired_versions": retired,
    })))
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TagBody {
    #[serde(default)]
    pub note: String,
}

async fn tag_layer_revisions(
    sa: SuperAdmin,
    State(st): State<AppState>,
    ApiPath(slug): ApiPath<String>,
    ApiJson(b): ApiJson<TagBody>,
) -> ApiResult<(StatusCode, Json<TagView>)> {
    let who = Who::of(&sa);
    let mut tx = write_tx(&st).await?;
    let head = template_head(&mut tx, &slug).await?;
    let family = head.key.clone();
    let note = match b.note.trim() {
        "" => {
            let n = rows::untagged_rows(&mut tx, &family).await?.len();
            format!("{n} revisions packaged.")
        }
        n => n.to_owned(),
    };
    let tag = tags::tag_family(
        &mut tx,
        &who.actor,
        &family,
        &note,
        &who.by,
        &*st.generator_guard,
        false,
    )
    .await?;
    let members = tags::tag_members(&mut tx, &family, tag.version).await?;
    let count = contents_of(&family, &members)?.len();
    let names = names(&mut tx, &BTreeSet::from([tag.tagged_by.clone()])).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(tag_view(&tag, count, &names))))
}

/// The retired versions of a family (`job_template_tag.retired_at`), ascending.
async fn retired_versions(conn: &mut PgConnection, family: &str) -> ApiResult<Vec<i32>> {
    Ok(sqlx::query_scalar(
        "SELECT version FROM job_template_tag WHERE layer = $1 AND retired_at IS NOT NULL ORDER BY version",
    )
    .bind(family)
    .fetch_all(conn)
    .await?)
}

async fn set_retired(
    sa: SuperAdmin,
    st: AppState,
    slug: String,
    raw_tag: String,
    retire: bool,
) -> ApiResult<Json<Value>> {
    let who = Who::of(&sa);
    let mut tx = write_tx(&st).await?;
    let head = template_head(&mut tx, &slug).await?;
    let family = head.key.clone();
    let version = parse_tag(&family, &raw_tag)?;
    let current: Option<bool> = sqlx::query_scalar(
        "SELECT retired_at IS NOT NULL FROM job_template_tag WHERE layer = $1 AND version = $2 FOR UPDATE",
    )
    .bind(&family)
    .bind(version)
    .fetch_optional(&mut *tx)
    .await?;
    let current = current.ok_or_else(|| {
        missing(
            "template_not_found",
            format!("{family} v{version} is not a registered job template."),
        )
    })?;
    if current == retire {
        return Err(ApiError::Conflict(
            "no_change",
            format!(
                "{family} v{version} is already {}.",
                if retire { "retired" } else { "offered" }
            ),
        ));
    }
    sqlx::query(
        "UPDATE job_template_tag SET retired_at = CASE WHEN $3 THEN now() END, \
                retired_by = CASE WHEN $3 THEN $4 END \
          WHERE layer = $1 AND version = $2",
    )
    .bind(&family)
    .bind(version)
    .bind(retire)
    .bind(&who.by)
    .execute(&mut *tx)
    .await?;
    audit::record_platform(
        &mut tx,
        &who.actor,
        PlatformEvent::new(
            if retire {
                "job_template.retire"
            } else {
                "job_template.restore"
            },
            &family,
        )
        .detail("family", family.as_str())
        .detail("version", version),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(
        json!({"layer_key": family, "version": version, "retired": retire}),
    ))
}

async fn retire_template(
    sa: SuperAdmin,
    State(st): State<AppState>,
    ApiPath((slug, tag)): ApiPath<(String, String)>,
) -> ApiResult<Json<Value>> {
    set_retired(sa, st, slug, tag, true).await
}

async fn restore_template(
    sa: SuperAdmin,
    State(st): State<AppState>,
    ApiPath((slug, tag)): ApiPath<(String, String)>,
) -> ApiResult<Json<Value>> {
    set_retired(sa, st, slug, tag, false).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_parse() {
        assert_eq!(parse_tag("base", "v4").unwrap(), 4);
        assert_eq!(parse_tag("base", "v0").unwrap(), 0);
        for bad in ["4", "v", "v-1", "vx", "V4", "v4a"] {
            assert_eq!(
                parse_tag("base", bad).unwrap_err().code(),
                "tag_not_found",
                "{bad}"
            );
        }
    }

    #[test]
    fn rollout_labels() {
        assert_eq!(rollout_label(Some(1)), Some("Customer care"));
        assert_eq!(rollout_label(Some(3)), Some("Other job areas"));
        assert_eq!(rollout_label(None), None);
        assert_eq!(rollout_label(Some(9)), None);
    }

    #[test]
    fn pins_behind() {
        let j = JobPins {
            tenant_id: Uuid::nil(),
            tenant_name: "T".into(),
            tenant_status: "active".into(),
            job: "care".into(),
            job_label: "Care".into(),
            pins: vec![
                StackEntry {
                    layer: "base".into(),
                    version: 2,
                },
                StackEntry {
                    layer: "job/care".into(),
                    version: 4,
                },
            ],
        };
        let p = pins_of(std::slice::from_ref(&j), "base", Some(3));
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].behind, 1);
        assert_eq!(
            pins_of(std::slice::from_ref(&j), "job/care", Some(4))[0].behind,
            0
        );
        assert!(pins_of(&[j], "industry/retail", Some(1)).is_empty());
    }
}
